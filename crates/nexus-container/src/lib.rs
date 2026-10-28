//! Opt-in container-backed tool environment and bounded worker protocol.
//!
//! `nexus-container` owns gateway/workload lifecycles, Docker transports,
//! container-specific tool composition, and the worker/keeper executables.
//! Ordinary host tools remain in `nexus-tools`; this crate depends on that
//! executor layer and never introduces a dependency in the opposite direction.

#![forbid(unsafe_code)]

mod exec;
pub mod file_tools;
pub mod gateway;
mod ownership;
mod session;
pub mod worker_client;
pub mod worker_protocol;
mod worker_transport;
mod workload;

pub use gateway::{
    ContainerEnvironment, ContainerError, GatewayIdentity, discover_host_ipv4_cidrs,
    validate_image_reference,
};
pub use workload::{tool_registrations, tool_registrations_with_settings};

/// Resolves a mutable local Docker reference to the immutable local image ID.
/// This only inspects the local daemon; it never pulls or builds an image.
pub fn resolve_local_image(reference: &str) -> Result<String, ContainerError> {
    let overrides = [
        "DOCKER_HOST",
        "DOCKER_CONTEXT",
        "DOCKER_TLS_VERIFY",
        "DOCKER_CERT_PATH",
    ]
    .map(|name| std::env::var_os(name).is_some());
    let config = std::env::var_os("DOCKER_CONFIG")
        .map(std::path::PathBuf::from)
        .map(std::fs::canonicalize)
        .transpose()
        .map_err(|_| ContainerError::UnsafeDockerContext)?;
    if config.as_ref().is_some_and(|path| !path.is_dir()) {
        return Err(ContainerError::UnsafeDockerContext);
    }
    let docker =
        workload::resolve_docker_executable().ok_or(ContainerError::InvalidDockerExecutable)?;
    resolve_local_image_with_policy(reference, &overrides, config.as_deref(), |args, config| {
        let command = image_inspection_command(&docker, config);
        gateway::run_bounded(command, args, std::time::Duration::from_secs(10))
    })
}

fn image_inspection_command(
    docker: &std::path::Path,
    config: Option<&std::path::Path>,
) -> std::process::Command {
    // Resolve the executable before removing PATH and other inherited variables.
    let mut command = std::process::Command::new(docker);
    command.env_clear().env("DOCKER_CONTEXT", "default");
    if let Some(config) = config {
        command.env("DOCKER_CONFIG", config);
    }
    command
}

fn resolve_local_image_with(
    reference: &str,
    inspect: impl FnOnce(&str) -> std::io::Result<String>,
) -> Result<String, ContainerError> {
    if reference.is_empty() || reference.bytes().any(|b| b.is_ascii_whitespace() || b == 0) {
        return Err(ContainerError::InvalidImage);
    }
    let id = inspect(reference).map_err(|_| ContainerError::InvalidImage)?;
    let id = id.strip_suffix('\n').unwrap_or(&id);
    if id.len() != 71
        || !id.starts_with("sha256:")
        || !id[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(ContainerError::InvalidImage);
    }
    Ok(id.to_owned())
}

fn resolve_local_image_with_policy(
    reference: &str,
    overrides: &[bool; 4],
    config: Option<&std::path::Path>,
    mut run: impl FnMut(&[&str], Option<&std::path::Path>) -> Result<Vec<u8>, ContainerError>,
) -> Result<String, ContainerError> {
    if reference.is_empty() || reference.bytes().any(|b| b.is_ascii_whitespace() || b == 0) {
        return Err(ContainerError::InvalidImage);
    }
    if overrides.iter().any(|present| *present) {
        return Err(ContainerError::UnsafeDockerContext);
    }
    let endpoint = run(
        &[
            "context",
            "inspect",
            "default",
            "--format",
            "{{.Endpoints.docker.Host}}",
        ],
        config,
    )?;
    let endpoint = String::from_utf8(endpoint).map_err(|_| ContainerError::UnsafeDockerContext)?;
    let endpoint = endpoint.trim();
    let local = endpoint
        .strip_prefix("unix://")
        .is_some_and(|socket| !socket.is_empty() && std::path::Path::new(socket).is_absolute())
        || endpoint.starts_with("npipe://./pipe/") && endpoint.len() > "npipe://./pipe/".len();
    if !local {
        return Err(ContainerError::UnsafeDockerContext);
    }
    let output = run(
        &["image", "inspect", "--format", "{{.Id}}", reference],
        config,
    )?;
    let output = String::from_utf8(output).map_err(|_| ContainerError::InvalidImage)?;
    resolve_local_image_with(reference, |_| Ok(output))
}

#[cfg(test)]
mod image_resolution_tests {
    use super::{resolve_local_image_with, resolve_local_image_with_policy};

    #[cfg(unix)]
    #[test]
    fn absolute_docker_executable_runs_with_cleared_path() {
        use std::os::unix::fs::PermissionsExt;

        let directory = std::env::temp_dir().join(format!(
            "nexus-image-inspect-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let docker = directory.join("docker");
        let id = format!("sha256:{}", "d".repeat(64));
        std::fs::write(
            &docker,
            format!(
                "#!/bin/sh\n[ \"$DOCKER_CONTEXT\" = default ] || exit 1\nprintf '%s\\n' '{id}'\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o700)).unwrap();
        let command = super::image_inspection_command(&docker, None);
        assert_eq!(command.get_program(), docker.as_os_str());
        assert!(!command.get_envs().any(|(name, _)| name == "PATH"));
        let result = super::gateway::run_bounded(
            command,
            &["image", "inspect", "--format", "{{.Id}}", "test:dev"],
            std::time::Duration::from_secs(2),
        );
        std::fs::remove_file(&docker).unwrap();
        std::fs::remove_dir(&directory).unwrap();
        assert_eq!(result.unwrap(), format!("{id}\n").as_bytes());
    }

    #[test]
    fn inspection_failures_preserve_docker_error_and_timeout() {
        for error in [
            super::ContainerError::DockerFailure,
            super::ContainerError::Timeout,
        ] {
            for fail_context in [true, false] {
                let result =
                    resolve_local_image_with_policy("image:tag", &[false; 4], None, |args, _| {
                        if fail_context || args[0] == "image" {
                            Err(error.clone())
                        } else {
                            Ok(b"unix:///var/run/docker.sock\n".to_vec())
                        }
                    });
                assert_eq!(result, Err(error.clone()));
            }
        }
    }

    #[test]
    fn resolves_only_by_local_inspection_and_never_pulls() {
        let id = format!("sha256:{}", "a".repeat(64));
        let resolved = resolve_local_image_with("gateway:dev", |reference| {
            assert_eq!(reference, "gateway:dev");
            Ok(id.clone())
        })
        .unwrap();
        assert_eq!(resolved, id);
    }

    #[test]
    fn missing_local_image_fails() {
        let error =
            resolve_local_image_with("missing:tag", |_| Err(std::io::ErrorKind::NotFound.into()));
        assert!(error.is_err());
    }

    #[test]
    fn accepts_only_a_single_exact_sha256_image_id() {
        let id = format!("sha256:{}", "b".repeat(64));
        for output in [id.clone(), format!("{id}\n")] {
            assert_eq!(
                resolve_local_image_with("repo:tag", |_| Ok(output)).unwrap(),
                id
            );
        }
        for output in [
            format!("repo@{id}"),
            format!("{id}\nextra"),
            format!("{id}\n\n"),
            format!("sha256:{}", "g".repeat(64)),
            format!("sha256:{}", "a".repeat(63)),
        ] {
            assert!(resolve_local_image_with("repo:tag", |_| Ok(output)).is_err());
        }
    }

    #[test]
    fn context_policy_prevents_image_inspect_until_default_endpoint_is_local() {
        let id = format!("sha256:{}", "c".repeat(64));
        let mut calls = Vec::new();
        let result = resolve_local_image_with_policy("image:tag", &[false; 4], None, |args, _| {
            calls.push(args[0].to_owned());
            Ok(b"tcp://remote.example:2376\n".to_vec())
        });
        assert!(result.is_err());
        assert_eq!(calls, ["context"]);

        let mut calls = Vec::new();
        let result = resolve_local_image_with_policy("image:tag", &[false; 4], None, |args, _| {
            calls.push(args[0].to_owned());
            if args[0] == "context" {
                Ok(b"unix:///var/run/docker.sock\n".to_vec())
            } else {
                Ok(id.as_bytes().to_vec())
            }
        })
        .unwrap();
        assert_eq!(result, id);
        assert_eq!(calls, ["context", "image"]);

        for index in 0..4 {
            let mut invoked = false;
            let mut overrides = [false; 4];
            overrides[index] = true;
            assert!(
                resolve_local_image_with_policy("image:tag", &overrides, None, |_, _| {
                    invoked = true;
                    Ok(Vec::new())
                })
                .is_err()
            );
            assert!(!invoked);
        }
    }
}
