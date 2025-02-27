use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use gpui::AsyncApp;
use http_client::github::{latest_github_release, GitHubLspBinaryVersion};
pub use language::*;
use lsp::{LanguageServerBinary, LanguageServerName};
use smol::fs::{self, File};
use std::{any::Any, env::consts, ffi::OsString, path::PathBuf, sync::Arc};
use util::{fs::remove_matching, maybe, ResultExt};

pub struct MarkdownLspAdapter;

fn server_binary_arguments() -> Vec<OsString> {
    vec!["server".into()]
}

impl MarkdownLspAdapter {
    const SERVER_NAME: LanguageServerName = LanguageServerName::new_static("marksman");

    fn build_asset_name() -> Result<String> {
        let suffix = match consts::OS {
            "macos" => "macos",
            "linux" => match consts::ARCH {
                "x86_64" => "linux-x64",
                "aarch64" | "arm64" => "linux-arm64",
                other => bail!("Running on unsupported architecture: {other}"),
            },
            "windows" => ".exe",
            other => bail!("Running on unsupported os: {other}"),
        };
        Ok(format!("marksman-{suffix}"))
    }
}

#[async_trait(?Send)]
impl super::LspAdapter for MarkdownLspAdapter {
    fn name(&self) -> LanguageServerName {
        Self::SERVER_NAME.clone()
    }

    async fn check_if_user_installed(
        &self,
        delegate: &dyn LspAdapterDelegate,
        _: Arc<dyn LanguageToolchainStore>,
        _: &AsyncApp,
    ) -> Option<LanguageServerBinary> {
        let path = delegate.which(Self::SERVER_NAME.as_ref()).await?;
        Some(LanguageServerBinary {
            path,
            env: None,
            arguments: server_binary_arguments(),
        })
    }

    async fn fetch_latest_server_version(
        &self,
        delegate: &dyn LspAdapterDelegate,
    ) -> Result<Box<dyn 'static + Send + Any>> {
        let release =
            latest_github_release("artempyanykh/marksman", true, false, delegate.http_client())
                .await?;
        let asset_name = MarkdownLspAdapter::build_asset_name()?;
        let asset = release
            .assets
            .iter()
            .find(|asset| asset.name == asset_name)
            .ok_or_else(|| anyhow!("no asset found matching {:?}", asset_name))?;
        let version = GitHubLspBinaryVersion {
            name: release.tag_name,
            url: asset.browser_download_url.clone(),
        };
        Ok(Box::new(version) as Box<_>)
    }

    async fn fetch_server_binary(
        &self,
        version: Box<dyn 'static + Send + Any>,
        container_dir: PathBuf,
        delegate: &dyn LspAdapterDelegate,
    ) -> Result<LanguageServerBinary> {
        let version = version.downcast::<GitHubLspBinaryVersion>().unwrap();
        let binary_path = container_dir.join(format!("marksman-{}", version.name));

        if fs::metadata(&binary_path).await.is_err() {
            let mut response = delegate
                .http_client()
                .get(&version.url, Default::default(), true)
                .await
                .context("error downloading release")?;
            let mut file = File::create(&binary_path).await?;
            if !response.status().is_success() {
                Err(anyhow!(
                    "download failed with status {}",
                    response.status().to_string()
                ))?;
            }
            futures::io::copy(response.body_mut(), &mut file).await?;

            #[cfg(not(windows))]
            {
                fs::set_permissions(
                    &binary_path,
                    <fs::Permissions as fs::unix::PermissionsExt>::from_mode(0o755),
                )
                .await?;
            }

            remove_matching(&container_dir, |entry| entry != binary_path).await;
        }

        Ok(LanguageServerBinary {
            path: binary_path,
            env: None,
            arguments: server_binary_arguments(),
        })
    }

    async fn cached_server_binary(
        &self,
        container_dir: PathBuf,
        _: &dyn LspAdapterDelegate,
    ) -> Option<LanguageServerBinary> {
        get_cached_server_binary(container_dir).await
    }

    async fn label_for_completion(
        &self,
        completion: &lsp::CompletionItem,
        _language: &Arc<Language>,
    ) -> Option<CodeLabel> {
        match completion.kind {
            Some(lsp::CompletionItemKind::REFERENCE) if completion.detail.is_some() => {
                let detail = completion.detail.as_ref().unwrap();
                let text = format!("{} - {}", detail, completion.label);
                Some(CodeLabel::plain(text, None))
            }
            _ => None,
        }
    }
}

async fn get_cached_server_binary(container_dir: PathBuf) -> Option<LanguageServerBinary> {
    maybe!(async {
        let mut last = None;
        let mut entries = fs::read_dir(&container_dir).await?;
        while let Some(entry) = entries.next().await {
            last = Some(entry?.path());
        }

        anyhow::Ok(LanguageServerBinary {
            path: last.ok_or_else(|| anyhow!("no cached marksman binary"))?,
            env: None,
            arguments: Default::default(),
        })
    })
    .await
    .log_err()
}
