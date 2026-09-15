//! Port of `app/searchengine.go` — `TestElasticsearch` and `PurgeElasticsearchIndexes`, the two
//! app functions behind `api4/elasticsearch.go`.
//!
//! # This server has no Elasticsearch engine, and that is the Team Edition's answer too
//!
//! `a.SearchEngine().ElasticsearchEngine` is registered only by the enterprise build
//! (`enterprise/local_imports.go`); on the Team Edition binary beside us it is nil, and both
//! functions end at the 501 `ent.elasticsearch.test_config.license.error`. There is no
//! Elasticsearch client in this port at all, so the same nil branch is taken here whatever the
//! licence says — an enterprise Go server with the engine loaded would go on to `TestConfig` or
//! `PurgeIndexes`, and that is the one thing about these two routes this build cannot compare.
//! What *is* ported is everything Go decides before the engine: the re-enter-password check
//! against the running configuration.

use mm_model::config::Config as ModelConfig;
use mm_model::utils::{AppError, AppResult, FAKE_SETTING};

use crate::App;

/// The 501 both functions answer when there is no engine.
fn no_engine(where_: &str) -> Box<AppError> {
    AppError::boxed(
        where_,
        "ent.elasticsearch.test_config.license.error",
        None,
        String::new(),
        501,
    )
}

/// The three `ElasticsearchSettings` `TestElasticsearch` reads off the configuration under
/// test, decoded as Go's decoder leaves them: a present value of the wrong type is the zero
/// string, since `encoding/json` allocates the pointer before it finds the type mismatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElasticsearchTestSettings {
    pub connection_url: String,
    pub username: String,
    pub password: String,
}

impl ElasticsearchTestSettings {
    /// `c.App.Config().ElasticsearchSettings`, for a body that decoded to nothing.
    pub fn from_running(running: &ModelConfig) -> Self {
        let es = &running.elasticsearch_settings;
        Self {
            connection_url: es.connection_url.clone().unwrap_or_default(),
            username: es.username.clone().unwrap_or_default(),
            password: es.password.clone().unwrap_or_default(),
        }
    }
}

impl App {
    /// Port of `App.TestElasticsearch` (searchengine.go:14).
    ///
    /// `cfg` is the configuration under test — the request body, or the running configuration
    /// when the body decoded to nothing — and `running` is `a.Config()`. A password of
    /// `model.FakeSetting` (what `GET /config` masks it to) is accepted only when the URL and
    /// username still match the running values, in which case the real password is substituted
    /// and the test goes on; otherwise it is the 400 `reenter_password`. **That check runs
    /// before the engine check**, so a masked password with a changed URL is a 400 even on a
    /// server with no engine, and a real password reaches the 501 straight away.
    ///
    /// Every pointer in `ElasticsearchSettings` has been proven non-nil by the handler
    /// (`checkHasNilFields`) before this is called; a running configuration whose document
    /// lacks one reads as `""`, which is the zero Go would have dereferenced to had the field
    /// been allocated.
    #[tracing::instrument(skip_all)]
    pub fn test_elasticsearch(
        &self,
        cfg: &mut ElasticsearchTestSettings,
        running: &ModelConfig,
    ) -> AppResult {
        if cfg.password == FAKE_SETTING {
            let running = ElasticsearchTestSettings::from_running(running);
            if cfg.connection_url == running.connection_url && cfg.username == running.username {
                cfg.password = running.password;
            } else {
                return Err(AppError::boxed(
                    "TestElasticsearch",
                    "ent.elasticsearch.test_config.reenter_password",
                    None,
                    String::new(),
                    400,
                ));
            }
        }

        Err(no_engine("TestElasticsearch"))
    }

    /// Port of `App.PurgeElasticsearchIndexes` (searchengine.go:39). The index list is read
    /// only past the engine check, which never passes here.
    #[tracing::instrument(skip_all, fields(indexes = indexes.len()))]
    pub fn purge_elasticsearch_indexes(&self, indexes: &[String]) -> AppResult {
        Err(no_engine("PurgeElasticsearchIndexes"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running(url: &str, user: &str, password: &str) -> ModelConfig {
        let mut config = ModelConfig::default();
        config.elasticsearch_settings.connection_url = Some(url.to_owned());
        config.elasticsearch_settings.username = Some(user.to_owned());
        config.elasticsearch_settings.password = Some(password.to_owned());
        config
    }

    /// The masked password is a 400 when the URL or the username moved, the real one is
    /// substituted when neither did, and a real password never looks at the running values.
    #[tokio::test]
    async fn the_reenter_password_check_precedes_the_engine() {
        let app = crate::App::new(mm_store::SqlStore::from_pool(
            sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://x/x")
                .unwrap(),
        ));
        let server = running("http://es:9200", "elastic", "secret");
        let mut same = ElasticsearchTestSettings {
            connection_url: "http://es:9200".to_owned(),
            username: "elastic".to_owned(),
            password: FAKE_SETTING.to_owned(),
        };
        let err = app.test_elasticsearch(&mut same, &server).unwrap_err();
        assert_eq!(err.status_code, 501);
        assert_eq!(same.password, "secret");

        let mut moved = ElasticsearchTestSettings {
            username: "other".to_owned(),
            ..same.clone()
        };
        moved.password = FAKE_SETTING.to_owned();
        let err = app.test_elasticsearch(&mut moved, &server).unwrap_err();
        assert_eq!(err.id, "ent.elasticsearch.test_config.reenter_password");
        assert_eq!(err.status_code, 400);

        let mut real = ElasticsearchTestSettings {
            connection_url: "http://elsewhere:9200".to_owned(),
            username: "nobody".to_owned(),
            password: "pw".to_owned(),
        };
        let err = app.test_elasticsearch(&mut real, &server).unwrap_err();
        assert_eq!(err.id, "ent.elasticsearch.test_config.license.error");
        assert_eq!(err.status_code, 501);
        assert_eq!(
            app.purge_elasticsearch_indexes(&[])
                .unwrap_err()
                .status_code,
            501
        );
    }
}
