use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use sui_compat::content_address::{
    make_fixed_output_path_from_ca, parse_content_address, ContentAddressMethod,
};
use sui_compat::hash::{HashAlgorithm, NixHash};
use sui_store::traits::{PathInfo, Store};

use super::dispatch::{absolute, op_err};
use super::wire::{
    read_bytes, read_framed_source, read_nar, read_string, read_string_list, read_u64, source_string,
    source_string_list, source_u64, write_stderr_data, write_string, write_string_list, write_u64, Direct, Tunnel,
};
use super::{Connection, ConnectionError};

const EXPORT_MAGIC: u64 = 0x4558_494e;

impl<S, R, W> Connection<S, R, W>
where
    S: Store + 'static,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    pub(super) async fn handle_add_to_store(&mut self) -> Result<(), ConnectionError> {
        if self.minor() >= 25 {
            let name = read_string(&mut self.reader).await?;
            let method = read_string(&mut self.reader).await?;
            let references = self.read_path_set().await?;
            let repair = read_u64(&mut self.reader).await? != 0;
            let dump = read_framed_source(&mut self.reader).await?;
            let (method, algo) = ContentAddressMethod::parse_with_algo(&method).map_err(op_err)?;
            let references: Vec<String> = references.iter().map(|p| p.to_absolute_path()).collect();
            let info = self
                .store
                .add_to_store_from_dump(&name, &dump, &method, algo, &references, repair)
                .await?;
            self.finish().await?;
            return self.write_valid_path_info(&info).await;
        }
        let name = read_string(&mut self.reader).await?;
        let fixed = read_u64(&mut self.reader).await? != 0;
        let recursive = read_u64(&mut self.reader).await?;
        let algo = read_string(&mut self.reader).await?;
        if recursive > 1 {
            return Err(ConnectionError::Refused(format!(
                "unsupported FileIngestionMethod with value of {recursive}; you may need to upgrade nix-daemon"
            )));
        }
        let (method, algo) = if fixed {
            (if recursive == 1 { ContentAddressMethod::Recursive } else { ContentAddressMethod::Flat }, algo)
        } else {
            (ContentAddressMethod::Recursive, "sha256".to_string())
        };
        let algo = HashAlgorithm::from_nix_str(&algo).map_err(|e| ConnectionError::Refused(e.to_string()))?;
        let nar = read_nar(&mut Direct(&mut self.reader)).await?;
        let dump = match method {
            ContentAddressMethod::Recursive => nar,
            _ => regular_contents(&nar).map_err(op_err)?,
        };
        let info = self.store.add_to_store_from_dump(&name, &dump, &method, algo, &[], false).await?;
        self.finish().await?;
        write_string(&mut self.writer, &info.path).await?;
        Ok(())
    }

    pub(super) async fn handle_add_text_to_store(&mut self) -> Result<(), ConnectionError> {
        let name = read_string(&mut self.reader).await?;
        let text = read_bytes(&mut self.reader).await?;
        let references = self.read_path_set().await?;
        let references: Vec<String> = references.iter().map(|p| p.to_absolute_path()).collect();
        let info = self
            .store
            .add_to_store_from_dump(&name, &text, &ContentAddressMethod::Text, HashAlgorithm::Sha256, &references, false)
            .await?;
        self.finish().await?;
        write_string(&mut self.writer, &info.path).await?;
        Ok(())
    }

    pub(super) async fn handle_add_to_store_nar(&mut self) -> Result<(), ConnectionError> {
        let path = self.read_path().await?.to_absolute_path();
        let deriver = read_string(&mut self.reader).await?;
        let nar_hash = read_string(&mut self.reader).await?;
        let references = self.read_path_set().await?;
        let registration_time = read_u64(&mut self.reader).await?;
        let nar_size = read_u64(&mut self.reader).await?;
        let _ultimate = read_u64(&mut self.reader).await?;
        let signatures = read_string_list(&mut self.reader).await?;
        let ca = read_string(&mut self.reader).await?;
        let repair = read_u64(&mut self.reader).await? != 0;
        let dont_check_sigs = read_u64(&mut self.reader).await? != 0 && self.trusted();
        let nar = if self.minor() >= 23 {
            read_framed_source(&mut self.reader).await?
        } else if self.minor() >= 21 {
            read_nar(&mut Tunnel::new(&mut self.reader, &mut self.writer)).await?
        } else {
            read_nar(&mut Direct(&mut self.reader)).await?
        };
        let info = PathInfo {
            path,
            nar_hash,
            nar_size: i64::try_from(nar_size).unwrap_or(i64::MAX),
            references: references.iter().map(|p| p.to_absolute_path()).collect(),
            deriver: (!deriver.is_empty()).then(|| absolute(&deriver)),
            signatures,
            registration_time: registration_time_or_now(registration_time),
            content_address: (!ca.is_empty()).then_some(ca),
        };
        self.import_path(&info, &nar, repair, !dont_check_sigs).await?;
        self.finish().await
    }

    pub(super) async fn handle_add_multiple_to_store(&mut self) -> Result<(), ConnectionError> {
        let repair = read_u64(&mut self.reader).await? != 0;
        let dont_check_sigs = read_u64(&mut self.reader).await? != 0 && self.trusted();
        let data = read_framed_source(&mut self.reader).await?;
        let mut cursor = std::io::Cursor::new(data.as_slice());
        let count = sui_compat::wire::read_u64(&mut cursor).map_err(op_err)?;
        for _ in 0..count {
            let path = sui_compat::wire::read_string(&mut cursor).map_err(op_err)?;
            self.parse_path(&path).map_err(|e| op_err(e.to_string()))?;
            let deriver = sui_compat::wire::read_string(&mut cursor).map_err(op_err)?;
            let nar_hash = sui_compat::wire::read_string(&mut cursor).map_err(op_err)?;
            let references = sui_compat::wire::read_string_list(&mut cursor).map_err(op_err)?;
            let registration_time = sui_compat::wire::read_u64(&mut cursor).map_err(op_err)?;
            let nar_size = sui_compat::wire::read_u64(&mut cursor).map_err(op_err)?;
            let _ultimate = sui_compat::wire::read_u64(&mut cursor).map_err(op_err)?;
            let signatures = sui_compat::wire::read_string_list(&mut cursor).map_err(op_err)?;
            let ca = sui_compat::wire::read_string(&mut cursor).map_err(op_err)?;
            let start = usize::try_from(cursor.position()).map_err(op_err)?;
            sui_compat::nar::NarReader::read_complete(&mut cursor).map_err(op_err)?;
            let end = usize::try_from(cursor.position()).map_err(op_err)?;
            let info = PathInfo {
                path,
                nar_hash,
                nar_size: i64::try_from(nar_size).unwrap_or(i64::MAX),
                references,
                deriver: (!deriver.is_empty()).then(|| absolute(&deriver)),
                signatures,
                registration_time: registration_time_or_now(registration_time),
                content_address: (!ca.is_empty()).then_some(ca),
            };
            self.import_path(&info, &data[start..end], repair, !dont_check_sigs).await?;
        }
        self.finish().await
    }

    pub(super) async fn import_path(
        &self,
        info: &PathInfo,
        nar: &[u8],
        repair: bool,
        check_sigs: bool,
    ) -> Result<(), ConnectionError> {
        if check_sigs && self.path_info_is_untrusted(info) {
            return Err(op_err(format!(
                "cannot add path '{}' because it lacks a signature by a trusted key",
                info.path
            )));
        }
        self.store.add_to_store_nar(info, nar, repair).await?;
        Ok(())
    }

    fn path_info_is_untrusted(&self, info: &PathInfo) -> bool {
        if !self.signatures.require_sigs || content_address_proves_path(info) {
            return false;
        }
        let fingerprint = sui_compat::signature::compute_fingerprint(
            &info.path,
            &info.nar_hash,
            u64::try_from(info.nar_size).unwrap_or(0),
            &info.references,
        );
        !sui_compat::signature::any_signature_trusted(&fingerprint, &info.signatures, &self.signatures.trusted_keys)
    }

    pub(super) async fn handle_nar_from_path(&mut self) -> Result<(), ConnectionError> {
        let sp = self.read_path().await?;
        self.finish().await?;
        let real = self.store_dir.join(sp.to_basename());
        let (tx, mut rx) = tokio::sync::mpsc::channel::<std::io::Result<Vec<u8>>>(16);
        let producer = tokio::task::spawn_blocking(move || {
            let mut sink = ChannelWriter { tx: tx.clone(), buf: Vec::with_capacity(CHUNK) };
            let result = sui_compat::nar::NarWriter::write_path(&mut sink, &real)
                .map_err(|e| std::io::Error::other(e.to_string()))
                .and_then(|()| std::io::Write::flush(&mut sink));
            if let Err(e) = result {
                let _ = tx.blocking_send(Err(e));
            }
        });
        while let Some(chunk) = rx.recv().await {
            match chunk {
                Ok(bytes) => self.writer.write_all(&bytes).await?,
                Err(e) => return Err(ConnectionError::Refused(format!("NarFromPath {}: {e}", sp.to_absolute_path()))),
            }
        }
        producer.await.map_err(|e| ConnectionError::Refused(e.to_string()))?;
        Ok(())
    }

    pub(super) async fn handle_export_path(&mut self) -> Result<(), ConnectionError> {
        let sp = self.read_path().await?;
        let _obsolete = read_u64(&mut self.reader).await?;
        let info = self.require_info(&sp).await?;
        let mut nar = Vec::new();
        sui_compat::nar::NarWriter::write_path(&mut nar, &self.store_dir.join(sp.to_basename())).map_err(op_err)?;
        let got = NixHash::of(HashAlgorithm::Sha256, &nar);
        if let Ok(recorded) = NixHash::parse_any(HashAlgorithm::Sha256, &info.nar_hash)
            && recorded != got
            && recorded.digest.iter().any(|b| *b != 0)
        {
            return Err(op_err(format!(
                "hash of path '{}' has changed from 'sha256:{}' to 'sha256:{}'!",
                info.path,
                recorded.to_nix32(),
                got.to_nix32()
            )));
        }
        let mut trailer = Vec::new();
        let mut references = info.references.clone();
        references.sort();
        sui_compat::wire::write_u64(&mut trailer, EXPORT_MAGIC).map_err(op_err)?;
        sui_compat::wire::write_string(&mut trailer, &info.path).map_err(op_err)?;
        sui_compat::wire::write_string_list(&mut trailer, &references).map_err(op_err)?;
        sui_compat::wire::write_string(&mut trailer, absolute(info.deriver.as_deref().unwrap_or(""))).map_err(op_err)?;
        sui_compat::wire::write_u64(&mut trailer, 0).map_err(op_err)?;
        for chunk in nar.chunks(CHUNK) {
            write_stderr_data(&mut self.writer, chunk).await?;
        }
        write_stderr_data(&mut self.writer, &trailer).await?;
        self.finish().await?;
        write_u64(&mut self.writer, 1).await?;
        Ok(())
    }

    pub(super) async fn handle_import_paths(&mut self) -> Result<(), ConnectionError> {
        let mut entries = Vec::new();
        {
            let mut tunnel = Tunnel::new(&mut self.reader, &mut self.writer);
            loop {
                match source_u64(&mut tunnel).await? {
                    0 => break,
                    1 => {}
                    _ => return Err(op_err("input doesn't look like something created by 'nix-store --export'")),
                }
                let nar = read_nar(&mut tunnel).await?;
                if source_u64(&mut tunnel).await? != EXPORT_MAGIC {
                    return Err(op_err("Nix archive cannot be imported; wrong format"));
                }
                let path = source_string(&mut tunnel).await?;
                let references = source_string_list(&mut tunnel).await?;
                let deriver = source_string(&mut tunnel).await?;
                if source_u64(&mut tunnel).await? == 1 {
                    source_string(&mut tunnel).await?;
                }
                entries.push((path, references, deriver, nar));
            }
        }
        let mut imported = Vec::with_capacity(entries.len());
        for (path, references, deriver, nar) in entries {
            self.parse_path(&path).map_err(|e| op_err(e.to_string()))?;
            let info = PathInfo {
                path: path.clone(),
                nar_hash: format!("sha256:{}", NixHash::of(HashAlgorithm::Sha256, &nar).to_hex()),
                nar_size: i64::try_from(nar.len()).unwrap_or(i64::MAX),
                references,
                deriver: (!deriver.is_empty()).then(|| absolute(&deriver)),
                signatures: Vec::new(),
                registration_time: registration_time_or_now(0),
                content_address: None,
            };
            self.import_path(&info, &nar, false, !self.trusted()).await?;
            imported.push(path);
        }
        self.finish().await?;
        write_string_list(&mut self.writer, &imported).await?;
        Ok(())
    }
}

const CHUNK: usize = 64 * 1024;

struct ChannelWriter {
    tx: tokio::sync::mpsc::Sender<std::io::Result<Vec<u8>>>,
    buf: Vec<u8>,
}

impl std::io::Write for ChannelWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= CHUNK {
            self.flush()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let chunk = std::mem::replace(&mut self.buf, Vec::with_capacity(CHUNK));
        self.tx
            .blocking_send(Ok(chunk))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "client went away"))
    }
}

fn regular_contents(nar: &[u8]) -> Result<Vec<u8>, String> {
    match sui_compat::nar::NarReader::read_complete(&mut std::io::Cursor::new(nar)) {
        Ok(sui_compat::nar::NarNode::Regular { contents, .. }) => Ok(contents),
        Ok(_) => Err("flat file ingestion requires a single regular file".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

fn registration_time_or_now(t: u64) -> i64 {
    if t != 0 {
        return i64::try_from(t).unwrap_or(i64::MAX);
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

fn content_address_proves_path(info: &PathInfo) -> bool {
    let Some(ca) = &info.content_address else {
        return false;
    };
    let Ok((method, hash)) = parse_content_address(ca) else {
        return false;
    };
    let name = super::dispatch::store_path_name(&info.path);
    let references: Vec<String> = info.references.iter().filter(|r| **r != info.path).cloned().collect();
    if references.len() != info.references.len() {
        return false;
    }
    make_fixed_output_path_from_ca(sui_compat::store_path::DEFAULT_STORE_DIR, &method, &hash, &references, &name)
        .is_ok_and(|p| p == info.path)
}
