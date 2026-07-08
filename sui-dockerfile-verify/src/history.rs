//! `docker history --no-trunc` comparison — layer count + instruction
//! order, driven through the same [`CommandRunner`] seam
//! `sui-dockerfile-wrapper` uses for `docker build`/`docker pull`.

use sui_dockerfile_wrapper::command::{CommandRunner, DockerBuildInvocation};

use crate::VerifyError;

/// Run `docker history --no-trunc --format '{{.CreatedBy}}' <image>`
/// and return one `CreatedBy` string per layer (newest layer first,
/// matching `docker history`'s own ordering).
///
/// # Errors
///
/// Returns [`VerifyError::Command`] if the subprocess couldn't be
/// spawned, or [`VerifyError::CommandFailed`] if `docker history`
/// exited non-zero (e.g. the image doesn't exist).
pub fn fetch_history<R: CommandRunner>(runner: &R, image_ref: &str) -> Result<Vec<String>, VerifyError> {
    let invocation = DockerBuildInvocation::history(image_ref);
    let outcome = runner.run(&invocation).map_err(VerifyError::Command)?;
    if !outcome.success {
        return Err(VerifyError::CommandFailed {
            image: image_ref.to_string(),
            stderr_tail: outcome.stderr_tail(4096),
        });
    }
    let stdout = String::from_utf8_lossy(&outcome.stdout);
    Ok(stdout.lines().map(ToString::to_string).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sui_dockerfile_wrapper::command::{CommandOutcome, MockCommandRunner};

    #[test]
    fn parses_one_created_by_per_line() {
        let runner = MockCommandRunner::with_outcome(CommandOutcome {
            success: true,
            exit_code: Some(0),
            stdout: b"CMD [\"/bin/true\"]\nRUN apt-get update\nFROM debian:bookworm-slim\n".to_vec(),
            stderr: Vec::new(),
        });

        let history = fetch_history(&runner, "example/image:test").unwrap();

        assert_eq!(history, vec!["CMD [\"/bin/true\"]", "RUN apt-get update", "FROM debian:bookworm-slim",]);
        let recorded = runner.recorded();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].args[0], "history");
        assert!(recorded[0].args.contains(&"--no-trunc".to_string()));
    }

    #[test]
    fn failing_docker_history_is_a_typed_error_not_a_panic() {
        let runner = MockCommandRunner::with_outcome(CommandOutcome {
            success: false,
            exit_code: Some(1),
            stdout: Vec::new(),
            stderr: b"Error: No such image: nope:latest".to_vec(),
        });

        let err = fetch_history(&runner, "nope:latest").unwrap_err();
        match err {
            VerifyError::CommandFailed { image, stderr_tail } => {
                assert_eq!(image, "nope:latest");
                assert!(stderr_tail.contains("No such image"));
            }
            other => panic!("expected CommandFailed, got {other:?}"),
        }
    }
}
