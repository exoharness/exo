use super::*;
use crate::resources::{PreparedResource, ResourceDefinition, ResourceSource, ResourceStore};
use std::path::Path;
use std::process::{Output, Stdio};
use tokio::io::AsyncWriteExt;

pub(super) async fn respond(
    root: &Path,
    request: Request<Incoming>,
) -> Result<Response<BoxBody<Bytes, Infallible>>> {
    let token = tokio::fs::read_to_string(root.join("expected-token")).await?;
    if request.uri().path() == "/repos/org/repo/pulls/10/reviews" {
        let authorized = request.headers().get("authorization").is_some_and(|value| {
            value.as_bytes() == format!("token {token}").as_bytes()
                || value.as_bytes() == format!("Bearer {token}").as_bytes()
        });
        return Ok(Response::builder()
            .status(if authorized {
                StatusCode::OK
            } else {
                StatusCode::UNAUTHORIZED
            })
            .header("content-type", "application/json")
            .body(
                Full::new(Bytes::from_static(if authorized {
                    br#"[{"body":"private review"}]"#
                } else {
                    br#"{"message":"Bad credentials"}"#
                }))
                .boxed(),
            )?);
    }
    let expected = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"))
    );
    let authenticated = request
        .headers()
        .get("authorization")
        .is_some_and(|value| value.as_bytes() == expected.as_bytes());
    if !authenticated {
        return Ok(Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header("www-authenticate", "Basic realm=git")
            .body(Full::new(Bytes::new()).boxed())?);
    }
    let mut child = tokio::process::Command::new("git")
        .arg("http-backend")
        .env("GIT_PROJECT_ROOT", root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("REMOTE_USER", "exo")
        .env("REQUEST_METHOD", request.method().as_str())
        .env("PATH_INFO", request.uri().path())
        .env("QUERY_STRING", request.uri().query().unwrap_or_default())
        .env(
            "CONTENT_TYPE",
            request
                .headers()
                .get("content-type")
                .map(|h| h.to_str())
                .transpose()?
                .unwrap_or_default(),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .context("Git backend stdin")?
        .write_all(&request.into_body().collect().await?.to_bytes())
        .await?;
    let output = child.wait_with_output().await?;
    ensure!(
        output.status.success(),
        "Git backend: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let separator = output
        .stdout
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .context("Git CGI headers")?;
    let mut response = Response::builder();
    for line in std::str::from_utf8(&output.stdout[..separator])?.split("\r\n") {
        let (name, value) = line.split_once(':').context("Git CGI header")?;
        if name.eq_ignore_ascii_case("status") {
            response = response.status(
                value
                    .trim()
                    .split(' ')
                    .next()
                    .context("Git CGI status")?
                    .parse::<u16>()?,
            );
        } else {
            response = response.header(name, value.trim());
        }
    }
    Ok(
        response
            .body(Full::new(Bytes::copy_from_slice(&output.stdout[separator + 4..])).boxed())?,
    )
}

async fn git(path: &Path, env: &HashMap<String, String>, args: &[&str]) -> Result<Output> {
    Ok(tokio::process::Command::new("git")
        .current_dir(path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .envs(env)
        .args(args)
        .output()
        .await?)
}

async fn success(path: &Path, env: &HashMap<String, String>, args: &[&str]) -> Result<String> {
    let output = git(path, env, args).await?;
    ensure!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

#[tokio::test]
async fn resource_git_push_uses_proxy_credentials_and_honors_revocation() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let origin = temp.path().join("authorized-repo.git");
    let workspace = temp.path().join("workspace");
    let empty = HashMap::new();
    success(
        temp.path(),
        &empty,
        &[
            "init",
            "--bare",
            "--initial-branch=main",
            origin.to_str().unwrap(),
        ],
    )
    .await?;
    success(
        temp.path(),
        &empty,
        &["init", "--initial-branch=main", workspace.to_str().unwrap()],
    )
    .await?;
    std::fs::write(workspace.join("README"), "preserved work")?;
    success(&workspace, &empty, &["add", "README"]).await?;
    success(
        &workspace,
        &empty,
        &[
            "-c",
            "user.name=Exo test",
            "-c",
            "user.email=exo@example.invalid",
            "commit",
            "-m",
            "saved commit",
        ],
    )
    .await?;
    let url = "https://api.test/authorized-repo.git";
    success(&workspace, &empty, &["remote", "add", "origin", url]).await?;
    let resource = PreparedResource {
        definition: ResourceDefinition {
            name: "code".into(),
            mount_path: workspace.to_string_lossy().into_owned(),
            mode: crate::FileSystemMountMode::ReadWrite,
            source: ResourceSource::GitRepository {
                path: None,
                url: Some(url.into()),
                checkout: None,
                credential: Some("test-credential".into()),
            },
        },
        snapshot: None,
    };
    let store = ResourceStore::new(&temp.path().join("state"))?;
    let agent_id = crate::Uuid7::now();
    let thread_id = crate::Uuid7::now();
    store.remember_external(agent_id, thread_id, Some(std::slice::from_ref(&resource)))?;
    let mounts = [crate::FileSystemMount {
        host_path: workspace.to_string_lossy().into_owned(),
        mount_path: resource.definition.mount_path.clone(),
        mode: crate::FileSystemMountMode::ReadWrite,
        internal: Some(true),
    }];
    let configured = store.command_env(
        crate::ResourceScope::Thread {
            agent_id,
            thread_id,
        },
        &mounts,
        empty.clone(),
    )?;
    std::fs::write(temp.path().join("expected-token"), "canary-v1")?;
    let upstream = Upstream::start_with_git(Some(temp.path().to_owned())).await?;
    let resolver = TestResolver::new();
    let mut policy = policy();
    policy.credentials[0].environment_variable = resource.definition.git_credential_variable();
    let SandboxNetworkPolicy::Limited { allowed_hosts } = &mut policy.networking else {
        unreachable!()
    };
    allowed_hosts.push("api.github.com".into());
    let mut github = policy.credentials[0].clone();
    github.environment_variable = "GH_TOKEN".into();
    github.networking = CredentialNetworkPolicy::Limited {
        allowed_hosts: vec!["api.github.com".into()],
    };
    policy.credentials.push(github);
    let proxy = super::super::explicit::ExplicitProxy::with_listener(
        State::new(
            identity("git"),
            policy,
            Some(resolver.clone()),
            Arc::new(upstream.config.clone()),
        )?,
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?,
        "127.0.0.1",
    )
    .await?;
    let ca = temp.path().join("ca.pem");
    std::fs::write(&ca, &proxy.ca_pem)?;
    let mut env = configured;
    env.extend(proxy.environment.clone());
    env.insert("GIT_SSL_CAINFO".into(), ca.to_string_lossy().into_owned());
    env.insert("SSL_CERT_FILE".into(), ca.to_string_lossy().into_owned());
    assert!(!format!("{env:?}").contains("canary"));

    success(
        &workspace,
        &env,
        &["push", "-u", "origin", "HEAD:refs/heads/saved-work"],
    )
    .await?;
    let head = success(&workspace, &empty, &["rev-parse", "HEAD"]).await?;
    assert_eq!(
        success(&origin, &empty, &["rev-parse", "refs/heads/saved-work"]).await?,
        head
    );
    *resolver.value.write().await = Some("canary-v2".into());
    std::fs::write(temp.path().join("expected-token"), "canary-v2")?;
    assert!(
        success(&workspace, &env, &["ls-remote", "origin"])
            .await?
            .contains(head.trim())
    );

    for other in [
        "https://public.test/authorized-repo.git",
        "https://api.test/other-repo.git",
    ] {
        let uses = resolver.uses.read().await.len();
        assert!(
            !git(&workspace, &env, &["ls-remote", other])
                .await?
                .status
                .success()
        );
        assert_eq!(
            resolver.uses.read().await.len(),
            uses,
            "credential used outside configured repository"
        );
    }
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(&proxy.environment["HTTPS_PROXY"])?)
        .add_root_certificate(reqwest::Certificate::from_pem(proxy.ca_pem.as_bytes())?)
        .build()?;
    let response = client
        .get("https://api.github.com/repos/org/repo/pulls/10/reviews")
        .bearer_auth(&env["GH_TOKEN"])
        .send()
        .await?
        .error_for_status()?;
    assert_eq!(response.text().await?, r#"[{"body":"private review"}]"#);
    assert_eq!(
        client
            .get("https://public.test/auth")
            .bearer_auth(&env["GH_TOKEN"])
            .send()
            .await?
            .status(),
        StatusCode::BAD_GATEWAY
    );
    let placeholder = &proxy.environment[&resource.definition.git_credential_variable()];
    for (host, token) in [
        ("public.test", placeholder.as_str()),
        ("api.test", "exo_egress_invalid"),
    ] {
        assert_eq!(
            client
                .get(format!("https://{host}/authorized-repo.git/info/refs"))
                .basic_auth("x-access-token", Some(token))
                .send()
                .await?
                .status(),
            StatusCode::BAD_GATEWAY
        );
    }
    *resolver.value.write().await = None;

    assert_eq!(
        client
            .get("https://api.github.com/repos/org/repo/pulls/10/reviews")
            .bearer_auth(&env["GH_TOKEN"])
            .send()
            .await?
            .status(),
        StatusCode::BAD_GATEWAY
    );
    assert!(
        !git(
            &workspace,
            &env,
            &["push", "origin", "HEAD:refs/heads/revoked"]
        )
        .await?
        .status
        .success()
    );
    assert!(
        !git(
            &origin,
            &empty,
            &["rev-parse", "--verify", "refs/heads/revoked"]
        )
        .await?
        .status
        .success()
    );
    assert!(!std::fs::read_to_string(workspace.join(".git/config"))?.contains("exo_egress"));
    proxy.close();
    Ok(())
}
