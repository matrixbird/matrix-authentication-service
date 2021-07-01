use std::sync::Arc;

use async_trait::async_trait;
use csrf::AesGcmCsrfProtection;
use tera::Tera;
use tide::{
    sessions::{MemoryStore, SessionMiddleware, SessionStore},
    Middleware,
};
use url::Url;

use crate::{config::Config, storage::Storage};

#[derive(Clone)]
pub struct State {
    config: Arc<Config>,
    templates: Arc<Tera>,
    storage: Arc<Storage>,
    session_store: Arc<MemoryStore>,
    csrf: Arc<AesGcmCsrfProtection>,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "State")
    }
}

impl State {
    pub fn new(config: Config, templates: Tera) -> Self {
        Self {
            config: Arc::new(config),
            templates: Arc::new(templates),
            storage: Default::default(),
            session_store: Arc::new(MemoryStore::new()),
            csrf: Arc::new(AesGcmCsrfProtection::from_key(
                *b"01234567012345670123456701234567",
            )),
        }
    }

    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    pub fn templates(&self) -> &Tera {
        &self.templates
    }

    pub fn csrf_protection(&self) -> Arc<AesGcmCsrfProtection> {
        self.csrf.clone()
    }

    pub fn session_middleware(self) -> impl Middleware<Self> {
        SessionMiddleware::new(self, b"some random value that we will figure out later")
    }

    fn base(&self) -> Url {
        self.config.oauth2.issuer.clone()
    }

    pub fn issuer(&self) -> Url {
        self.base()
    }

    pub fn authorization_endpoint(&self) -> Option<Url> {
        self.base().join("oauth2/authorize").ok()
    }

    pub fn token_endpoint(&self) -> Option<Url> {
        self.base().join("oauth2/token").ok()
    }

    pub fn jwks_uri(&self) -> Option<Url> {
        self.base().join(".well-known/jwks.json").ok()
    }
}

#[async_trait]
impl SessionStore for State {
    async fn load_session(
        &self,
        cookie_value: String,
    ) -> anyhow::Result<Option<tide::sessions::Session>> {
        self.session_store.load_session(cookie_value).await
    }

    async fn store_session(
        &self,
        session: tide::sessions::Session,
    ) -> anyhow::Result<Option<String>> {
        self.session_store.store_session(session).await
    }

    async fn destroy_session(&self, session: tide::sessions::Session) -> anyhow::Result<()> {
        self.session_store.destroy_session(session).await
    }

    async fn clear_store(&self) -> anyhow::Result<()> {
        self.session_store.clear_store().await
    }
}
