use super::*;

#[derive(Serialize, Deserialize)]
struct LegacyCatalog {
    vaults: Vec<LegacyVault>,
}
#[derive(Serialize, Deserialize)]
struct LegacyVault {
    record: VaultRecord,
    secrets: Vec<LegacySecret>,
}
#[derive(Serialize, Deserialize)]
struct LegacySecret {
    metadata: LegacyMetadata,
    secret: EncryptedSecret,
}

// These serialized fields are the authenticated data for vaults written before
// credential policies. Decrypt with the original bytes before changing metadata.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target: Option<LegacyTarget>,
    #[serde(default = "crate::types::initial_secret_revision")]
    revision: u64,
    id: SecretId,
    r#type: SecretType,
    name: String,
    created_at: DateTimeUtc,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum LegacyTarget {
    Mcp { server_url: String },
    Http { origin: String },
}

pub(super) fn read_catalog(bytes: &[u8], cipher: &SecretCipher) -> Result<(Catalog, bool)> {
    if let Ok(catalog) = serde_json::from_slice::<Catalog>(bytes) {
        return Ok((catalog, false));
    }
    let old: LegacyCatalog = serde_json::from_slice(bytes).context("reading vault catalog")?;
    let mut catalog = Catalog::default();
    for vault in old.vaults {
        let mut secrets = Vec::new();
        for stored in vault.secrets {
            let secret = cipher.decrypt_bound(
                &stored.secret,
                &serde_json::to_vec(&(vault.record.id, &stored.metadata))?,
            )?;
            let policy = stored
                .metadata
                .target
                .map(|target| match target {
                    LegacyTarget::Mcp { server_url } => CredentialDestination::url(&server_url),
                    LegacyTarget::Http { origin } => CredentialDestination::origin(&origin),
                })
                .transpose()?
                .map(Into::into);
            let metadata = SecretMetadata {
                policy,
                revision: stored.metadata.revision,
                id: stored.metadata.id,
                r#type: stored.metadata.r#type,
                name: stored.metadata.name,
                created_at: stored.metadata.created_at,
            };
            let secret = encrypt(cipher, vault.record.id, &metadata, &secret)?;
            secrets.push(StoredSecret { metadata, secret });
        }
        catalog.vaults.push(StoredVault {
            record: vault.record,
            secrets,
        });
    }
    Ok((catalog, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BasicExoHarness;

    #[tokio::test]
    async fn migrates_authenticated_metadata_without_losing_credentials_or_widening_scope()
    -> Result<()> {
        let temp = tempfile::tempdir()?;
        let config = crate::test_support::local_test_config(temp.path());
        let cipher = crate::basic::build_secret_cipher(
            config.secret_backend.clone(),
            temp.path().to_string_lossy().into_owned(),
        )?;
        let record = VaultRecord {
            id: Uuid7::now(),
            name: "personal".into(),
            created_at: Utc::now(),
        };
        let mut secrets = Vec::new();
        for target in [
            Some(LegacyTarget::Http {
                origin: "https://github.com".into(),
            }),
            Some(LegacyTarget::Mcp {
                server_url: "https://notion.test/mcp/?a=1".into(),
            }),
            None,
        ] {
            let metadata = LegacyMetadata {
                target,
                revision: 7,
                id: Uuid7::now(),
                r#type: SecretType::Key,
                name: format!("credential-{}", secrets.len()),
                created_at: Utc::now(),
            };
            let secret = cipher.encrypt_bound(
                &Secret::Key {
                    value: "migration-canary".into(),
                },
                &serde_json::to_vec(&(record.id, &metadata))?,
            )?;
            secrets.push(LegacySecret { metadata, secret });
        }
        let ids: Vec<_> = secrets.iter().map(|s| s.metadata.id).collect();
        let path = temp.path().join("vaults/vaults.json");
        std::fs::create_dir_all(path.parent().unwrap())?;
        let catalog = LegacyCatalog {
            vaults: vec![LegacyVault {
                record: record.clone(),
                secrets,
            }],
        };
        std::fs::write(&path, serde_json::to_vec(&catalog)?)?;
        let harness = BasicExoHarness::new(config.clone()).await?;
        let vault = harness.get_vault(&record.id).await?.unwrap();
        for id in &ids {
            assert_eq!(
                vault.get_secret(id).await?,
                Some(Secret::Key {
                    value: "migration-canary".into()
                })
            );
        }
        assert!(vault.list_secrets().await?.iter().all(|s| s.revision == 7));
        vault
            .resolve_secret(
                &ids[0],
                &CredentialDestination::url("https://github.com/repo")?,
            )
            .await?;
        assert!(
            vault
                .resolve_secret(
                    &ids[0],
                    &CredentialDestination::url("https://api.github.com/repo")?
                )
                .await
                .is_err()
        );
        vault
            .resolve_secret(
                &ids[1],
                &CredentialDestination::url("https://notion.test/mcp/?a=1")?,
            )
            .await?;
        for url in [
            "https://notion.test/mcp",
            "https://notion.test/mcp/?a=2",
            "https://notion.test:8443/mcp/?a=1",
        ] {
            assert!(
                vault
                    .resolve_secret(&ids[1], &CredentialDestination::url(url)?)
                    .await
                    .is_err()
            );
        }
        assert!(!std::fs::read_to_string(&path)?.contains("migration-canary"));
        let reopened = BasicExoHarness::new(config).await?;
        reopened
            .get_vault(&record.id)
            .await?
            .unwrap()
            .resolve_secret(
                &ids[0],
                &CredentialDestination::origin("https://github.com")?,
            )
            .await?;
        let mut tampered = catalog;
        tampered.vaults[0].secrets[0].metadata.name = "tampered".into();
        assert!(read_catalog(&serde_json::to_vec(&tampered)?, &cipher).is_err());
        Ok(())
    }
}
