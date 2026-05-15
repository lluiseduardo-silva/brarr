//! Parsing do arquivo de configuração TOML + resolução do path default.
//!
//! Formato esperado:
//!
//! ```toml
//! [[tracker]]
//! name = "capybara"
//! base_url = "https://capybarabr.com/"
//! token = "redacted-token"
//!
//! [[tracker]]
//! name = "locadora"
//! base_url = "https://locadora.cc/"
//! token = "redacted-token"
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use brarr_decision_service::Rule;
use serde::Deserialize;
use url::Url;

/// Configuração completa parseada do TOML.
///
/// Aceita opcionalmente `[[rule]]` blocos que substituem o scoring
/// default — veja [`brarr_decision_service`] para o schema. Quando
/// ausentes, [`Engine::baseline`](brarr_decision_service::Engine::baseline)
/// é usado em [`main.rs`](../main.rs.html).
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Lista de trackers configurados. Mapeia para `[[tracker]]` no TOML.
    #[serde(rename = "tracker", default)]
    pub trackers: Vec<TrackerConfig>,
    /// Regras opcionais do motor de decisão. Mapeia para `[[rule]]` no TOML.
    /// Vazio → caller usa [`Engine::baseline`].
    #[serde(rename = "rule", default)]
    pub rules: Vec<Rule>,
}

/// Configuração de um único tracker `UNIT3D`.
#[derive(Debug, Clone, Deserialize)]
pub struct TrackerConfig {
    /// Nome de display (não-vazio).
    pub name: String,
    /// URL base da API (e.g., `https://capybarabr.com/`).
    pub base_url: Url,
    /// Bearer token de API. Tratado como segredo.
    pub token: String,
}

/// Erros possíveis ao carregar config.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Arquivo não existe no path indicado.
    #[error("config file not found at {0}")]
    NotFound(PathBuf),

    /// I/O genérico (permissão negada, etc.).
    #[error("could not read config at {path}: {source}")]
    Read {
        /// Path tentado.
        path: PathBuf,
        /// Erro do filesystem.
        #[source]
        source: std::io::Error,
    },

    /// TOML malformado.
    #[error("invalid TOML at {path}: {source}")]
    Parse {
        /// Path do arquivo problemático.
        path: PathBuf,
        /// Erro original do toml crate.
        #[source]
        source: toml::de::Error,
    },

    /// `directories::ProjectDirs::from(...)` retornou `None` (sistema
    /// sem `$HOME` configurado, e.g., um chroot ou container minimal).
    #[error("could not determine default config directory; pass --config explicitly")]
    NoDefaultDir,

    /// Config válido mas sem trackers — uso é praticamente o mesmo que
    /// "config faltando" e é melhor falhar early com mensagem clara.
    #[error("config contains no trackers — add at least one [[tracker]] entry")]
    NoTrackers,
}

impl Config {
    /// Carrega e valida config a partir do path indicado.
    ///
    /// # Errors
    ///
    /// Veja [`ConfigError`].
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ConfigError::NotFound(path.to_path_buf()));
            }
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        let cfg: Self = toml::from_str(&content).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        if cfg.trackers.is_empty() {
            return Err(ConfigError::NoTrackers);
        }
        Ok(cfg)
    }

    /// Resolve o path default da config para a plataforma corrente.
    ///
    /// # Errors
    ///
    /// [`ConfigError::NoDefaultDir`] se não der pra resolver via
    /// `directories::ProjectDirs` (typically sistema sem `$HOME`).
    pub fn default_path() -> Result<PathBuf, ConfigError> {
        directories::ProjectDirs::from("", "", "brarr")
            .map(|d| d.config_dir().join("config.toml"))
            .ok_or(ConfigError::NoDefaultDir)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{Config, ConfigError};
    use tempfile_stub::NamedTempFile;

    /// Fallback bem simples para criar um arquivo temporário com conteúdo
    /// sem trazer a dep `tempfile`. Cria em `std::env::temp_dir()` com
    /// nome único, limpa no Drop.
    mod tempfile_stub {
        use std::fs;
        use std::io;
        use std::path::PathBuf;

        pub struct NamedTempFile {
            path: PathBuf,
        }

        impl NamedTempFile {
            pub fn new() -> io::Result<Self> {
                let dir = std::env::temp_dir();
                let pid = std::process::id();
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos());
                let path = dir.join(format!("brarr-cfg-test-{pid}-{nanos}.toml"));
                fs::write(&path, "")?;
                Ok(Self { path })
            }
            pub fn path(&self) -> &std::path::Path {
                &self.path
            }
            pub fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
                fs::write(&self.path, data)
            }
        }

        impl Drop for NamedTempFile {
            fn drop(&mut self) {
                let _ = fs::remove_file(&self.path);
            }
        }
    }

    fn temp_with(contents: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().expect("temp file");
        f.write_all(contents.as_bytes()).expect("write");
        f
    }

    #[test]
    fn loads_minimal_valid_config() {
        let f = temp_with(
            r#"
[[tracker]]
name = "capybara"
base_url = "https://capybarabr.com/"
token = "abc"
"#,
        );
        let cfg = Config::load(f.path()).expect("valid");
        assert_eq!(cfg.trackers.len(), 1);
        assert_eq!(cfg.trackers[0].name, "capybara");
        assert_eq!(cfg.trackers[0].token, "abc");
        assert_eq!(cfg.trackers[0].base_url.as_str(), "https://capybarabr.com/",);
    }

    #[test]
    fn loads_multiple_trackers() {
        let f = temp_with(
            r#"
[[tracker]]
name = "capybara"
base_url = "https://capybarabr.com/"
token = "a"

[[tracker]]
name = "locadora"
base_url = "https://locadora.cc/"
token = "b"
"#,
        );
        let cfg = Config::load(f.path()).expect("valid");
        assert_eq!(cfg.trackers.len(), 2);
        assert_eq!(cfg.trackers[1].name, "locadora");
    }

    #[test]
    fn rejects_empty_trackers_list() {
        let f = temp_with("");
        let err = Config::load(f.path()).expect_err("no trackers");
        assert!(matches!(err, ConfigError::NoTrackers));
    }

    #[test]
    fn surfaces_missing_file_clearly() {
        let path = std::env::temp_dir().join("brarr-cfg-missing-xyzzy.toml");
        let _ = std::fs::remove_file(&path);
        let err = Config::load(&path).expect_err("missing");
        assert!(matches!(err, ConfigError::NotFound(_)));
    }

    #[test]
    fn surfaces_malformed_toml() {
        let f = temp_with("this = is = not valid TOML =");
        let err = Config::load(f.path()).expect_err("bad toml");
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn rejects_invalid_base_url() {
        // serde + url::Url falham na desserialização se a URL não parsear.
        let f = temp_with(
            r#"
[[tracker]]
name = "x"
base_url = "not a url"
token = "t"
"#,
        );
        let err = Config::load(f.path()).expect_err("bad url");
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn default_path_resolves_under_brarr() {
        // Pode falhar em sistemas exóticos sem $HOME; nesse caso vira
        // NoDefaultDir, que é tratado.
        //
        // O nome exato varia por plataforma:
        // - Linux: `$XDG_CONFIG_HOME/brarr/config.toml`
        // - macOS: `~/Library/Application Support/brarr/config.toml`
        // - Windows: `%APPDATA%\brarr\config\config.toml`
        //   (o `directories` mete um sub-`config\` extra no Windows)
        //
        // O invariante testável é: o path contém "brarr" e termina em
        // "config.toml".
        match Config::default_path() {
            Ok(p) => {
                let s = p.to_string_lossy();
                assert!(s.contains("brarr"), "expected 'brarr' in path: {s}");
                assert!(
                    p.file_name().is_some_and(|n| n == "config.toml"),
                    "expected file name config.toml: {s}",
                );
            }
            Err(ConfigError::NoDefaultDir) => {} // env sem HOME — tudo bem
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
}
