use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoplinSession {
    pub id: String,
    pub user_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoplinUser {
    pub id: String,
    pub email: String,
}

#[async_trait]
pub trait JoplinAuthenticator: Send + Sync {
    async fn authenticate(&self, email: &str, password: &str) -> anyhow::Result<JoplinSession>;
    async fn invalidate_session(&self, session_id: &str) -> anyhow::Result<()>;
}

#[derive(Debug, Clone)]
pub struct HttpJoplinAuthenticator {
    client: reqwest::Client,
    base_url: url::Url,
}

impl HttpJoplinAuthenticator {
    pub fn new(base_url: url::Url) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url,
        }
    }
}

#[async_trait]
impl JoplinAuthenticator for HttpJoplinAuthenticator {
    async fn authenticate(&self, email: &str, password: &str) -> anyhow::Result<JoplinSession> {
        #[derive(Serialize)]
        struct AuthRequest<'a> {
            email: &'a str,
            password: &'a str,
        }

        let url = self.base_url.join("/api/sessions")?;
        let response = self
            .client
            .post(url)
            .json(&AuthRequest { email, password })
            .send()
            .await?;

        if !response.status().is_success() {
            anyhow::bail!("Joplin authentication failed");
        }

        Ok(response.json::<JoplinSession>().await?)
    }

    async fn invalidate_session(&self, session_id: &str) -> anyhow::Result<()> {
        let url = self.base_url.join(&format!("/api/sessions/{session_id}"))?;
        let response = self.client.delete(url).send().await?;
        if response.status().is_success() || response.status().as_u16() == 404 {
            return Ok(());
        }
        anyhow::bail!("Joplin session invalidation failed");
    }
}
