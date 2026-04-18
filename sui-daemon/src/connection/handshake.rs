//! Worker protocol handshake.
//!
//! Implements the initial magic / version / trust negotiation that runs
//! once per accepted connection, before the opcode dispatch loop.

use tokio::io::{AsyncWrite, AsyncWriteExt};

use sui_compat::wire::{PROTOCOL_VERSION, WORKER_MAGIC_1, WORKER_MAGIC_2};

use super::wire::{read_u64, write_stderr_last, write_string, write_u64};
use super::{
    Connection, ConnectionError, PROTOCOL_MINOR_CPU_AFFINITY, PROTOCOL_MINOR_RESERVE_SPACE,
    PROTOCOL_MINOR_TRUST_EXCHANGE,
};

use sui_store::traits::Store;
use tokio::io::AsyncRead;

impl<S, R, W> Connection<S, R, W>
where
    S: Store,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Perform the Nix worker protocol handshake.
    ///
    /// Sequence:
    /// 1. Read `WORKER_MAGIC_1` from client
    /// 2. Write `WORKER_MAGIC_2` to client
    /// 3. Write `PROTOCOL_VERSION` to client
    /// 4. Read client protocol version
    /// 5. Write `0_u64` for CPU affinity (obsolete)
    /// 6. Write `0_u64` for reserve space (obsolete)
    /// 7. Write the daemon version string
    /// 8. Exchange trust level
    pub async fn handshake(&mut self) -> Result<(), ConnectionError> {
        // 1. Read client magic
        let magic = read_u64(&mut self.reader).await?;
        if magic != WORKER_MAGIC_1 {
            return Err(ConnectionError::BadMagic {
                expected: WORKER_MAGIC_1,
                got: magic,
            });
        }

        // 2. Write server magic
        write_u64(&mut self.writer, WORKER_MAGIC_2).await?;

        // 3. Write server protocol version
        write_u64(&mut self.writer, PROTOCOL_VERSION).await?;
        self.writer.flush().await?;

        // 4. Read client protocol version
        self.client_version = read_u64(&mut self.reader).await?;

        // 5. Obsolete CPU affinity (must still send zero)
        if self.client_version >= PROTOCOL_MINOR_CPU_AFFINITY {
            let _cpu_affinity = read_u64(&mut self.reader).await?;
        }

        // 6. Obsolete reserve space (must still send zero)
        if self.client_version >= PROTOCOL_MINOR_RESERVE_SPACE {
            let _reserve = read_u64(&mut self.reader).await?;
        }

        // 7. Write daemon version string
        write_string(&mut self.writer, "sui-daemon 0.1.0").await?;

        // 8. Write trust level
        if self.client_version >= PROTOCOL_MINOR_TRUST_EXCHANGE {
            write_u64(&mut self.writer, u64::from(self.trust)).await?;
        }

        // 9. Terminate the handshake with STDERR_LAST.
        //
        // This is the step discovered via end-to-end testing against
        // real `nix-store`: CppNix clients issue their first opcode
        // ONLY after seeing `STDERR_LAST` following the trust-flag
        // write. Without it, the client sits in a blocking read
        // forever. Missing this was the reason
        // `tests/real_nix_client.rs` hung for 60+ seconds on every
        // invocation despite the handshake "completing" server-side.
        //
        // The symmetry with per-op replies — every handler already
        // terminates its own response with `write_stderr_last` — is
        // load-bearing: the handshake is effectively op-zero.
        write_stderr_last(&mut self.writer).await?;

        self.writer.flush().await?;

        tracing::info!(
            client_version = self.client_version,
            trust = %self.trust,
            "handshake complete"
        );

        Ok(())
    }
}
