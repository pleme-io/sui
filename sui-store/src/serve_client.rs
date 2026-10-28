use std::io::{BufReader, BufWriter, Read, Write};

use sui_compat::wire::{self, ServeCommand, SERVE_MAGIC_1, SERVE_MAGIC_2, SERVE_PROTOCOL_VERSION};

use crate::worker_client::{RemoteBuildResult, RemotePathInfo, Transport, WorkerError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BuildOptions {
    pub max_silent_time: u64,
    pub build_timeout: u64,
    pub max_log_size: u64,
    pub keep_failed: bool,
}

pub struct ServeConn {
    reader: BufReader<Box<dyn Read + Send>>,
    writer: BufWriter<Box<dyn Write + Send>>,
    minor: u64,
    host: String,
}

impl ServeConn {
    pub fn over_transport((reader, writer): Transport, host: &str) -> Result<Self, WorkerError> {
        let mut conn = Self {
            reader: BufReader::new(reader),
            writer: BufWriter::new(writer),
            minor: SERVE_PROTOCOL_VERSION & 0xff,
            host: host.to_string(),
        };
        wire::write_u64(&mut conn.writer, SERVE_MAGIC_1)?;
        wire::write_u64(&mut conn.writer, SERVE_PROTOCOL_VERSION)?;
        conn.writer.flush()?;
        if wire::read_u64(&mut conn.reader)? != SERVE_MAGIC_2 {
            return Err(WorkerError::Protocol(format!("'nix-store --serve' protocol mismatch from '{host}'")));
        }
        let remote = wire::read_u64(&mut conn.reader)?;
        if remote & 0xff00 != 0x200 {
            return Err(WorkerError::Protocol(format!("unsupported 'nix-store --serve' protocol version on '{host}'")));
        }
        conn.minor = (remote & 0xff).min(SERVE_PROTOCOL_VERSION & 0xff);
        Ok(conn)
    }

    #[must_use]
    pub fn minor(&self) -> u64 {
        self.minor
    }

    fn cmd(&mut self, c: ServeCommand) -> Result<(), WorkerError> {
        Ok(wire::write_u64(&mut self.writer, c as u64)?)
    }

    pub fn query_valid_paths(&mut self, paths: &[String], lock: bool, substitute: bool) -> Result<Vec<String>, WorkerError> {
        self.cmd(ServeCommand::QueryValidPaths)?;
        wire::write_u64(&mut self.writer, u64::from(lock))?;
        wire::write_u64(&mut self.writer, u64::from(substitute))?;
        wire::write_string_list(&mut self.writer, paths)?;
        self.writer.flush()?;
        Ok(wire::read_string_list(&mut self.reader)?)
    }

    pub fn query_path_infos(&mut self, paths: &[String]) -> Result<Vec<RemotePathInfo>, WorkerError> {
        self.cmd(ServeCommand::QueryPathInfos)?;
        wire::write_string_list(&mut self.writer, paths)?;
        self.writer.flush()?;
        let mut out = Vec::new();
        loop {
            let path = wire::read_string(&mut self.reader)?;
            if path.is_empty() {
                return Ok(out);
            }
            let deriver = wire::read_string(&mut self.reader)?;
            let references = wire::read_string_list(&mut self.reader)?;
            let _download_size = wire::read_u64(&mut self.reader)?;
            let nar_size = wire::read_u64(&mut self.reader)?;
            let (nar_hash, ca, sigs) = if self.minor >= 4 {
                (wire::read_string(&mut self.reader)?, wire::read_string(&mut self.reader)?, wire::read_string_list(&mut self.reader)?)
            } else {
                (String::new(), String::new(), Vec::new())
            };
            out.push(RemotePathInfo { path, deriver, nar_hash, references, registration_time: 0, nar_size, ultimate: false, sigs, ca });
        }
    }

    pub fn add_to_store_nar(&mut self, info: &RemotePathInfo, nar: &[u8]) -> Result<(), WorkerError> {
        if self.minor < 5 {
            return Err(WorkerError::Protocol(format!("'{}' speaks serve protocol 2.{}, AddToStoreNar needs 2.5", self.host, self.minor)));
        }
        let hash = info.nar_hash.split_once(':').map_or(info.nar_hash.as_str(), |(_, h)| h).to_string();
        self.cmd(ServeCommand::AddToStoreNar)?;
        wire::write_string(&mut self.writer, &info.path)?;
        wire::write_string(&mut self.writer, &info.deriver)?;
        wire::write_string(&mut self.writer, &hash)?;
        wire::write_string_list(&mut self.writer, &info.references)?;
        wire::write_u64(&mut self.writer, info.registration_time)?;
        wire::write_u64(&mut self.writer, info.nar_size)?;
        wire::write_u64(&mut self.writer, u64::from(info.ultimate))?;
        wire::write_string_list(&mut self.writer, &info.sigs)?;
        wire::write_string(&mut self.writer, &info.ca)?;
        self.writer.write_all(nar)?;
        self.writer.flush()?;
        if wire::read_u64(&mut self.reader)? != 1 {
            return Err(WorkerError::Daemon(format!("failed to add path '{}' to remote host '{}'", info.path, self.host)));
        }
        Ok(())
    }

    pub fn nar_from_path(&mut self, path: &str) -> Result<Vec<u8>, WorkerError> {
        self.cmd(ServeCommand::DumpStorePath)?;
        wire::write_string(&mut self.writer, path)?;
        self.writer.flush()?;
        crate::worker_client::read_nar_bytes(&mut self.reader, path)
    }

    pub fn build_derivation(
        &mut self,
        drv_path: &str,
        drv: &sui_compat::derivation::Derivation,
        input_srcs: &[String],
        options: BuildOptions,
    ) -> Result<RemoteBuildResult, WorkerError> {
        self.cmd(ServeCommand::BuildDerivation)?;
        wire::write_string(&mut self.writer, drv_path)?;
        crate::worker_client::write_basic_derivation(&mut self.writer, drv, input_srcs)?;
        wire::write_u64(&mut self.writer, options.max_silent_time)?;
        wire::write_u64(&mut self.writer, options.build_timeout)?;
        if self.minor >= 2 {
            wire::write_u64(&mut self.writer, options.max_log_size)?;
        }
        if self.minor >= 3 {
            wire::write_u64(&mut self.writer, 0)?;
            wire::write_u64(&mut self.writer, 0)?;
        }
        if self.minor >= 7 {
            wire::write_u64(&mut self.writer, u64::from(options.keep_failed))?;
        }
        self.writer.flush()?;
        let status = wire::read_u64(&mut self.reader)?;
        let error_msg = wire::read_string(&mut self.reader)?;
        if self.minor >= 3 {
            for _ in 0..4 {
                wire::read_u64(&mut self.reader)?;
            }
        }
        if self.minor >= 6 {
            let n = wire::read_u64(&mut self.reader)?;
            for _ in 0..n {
                wire::read_string(&mut self.reader)?;
                wire::read_string(&mut self.reader)?;
            }
        }
        Ok(RemoteBuildResult { status, error_msg })
    }
}
