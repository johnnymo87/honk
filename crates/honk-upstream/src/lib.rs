use reqwest::Url;

/// Production goose serve port; probes must never talk to it.
pub const PRODUCTION_GOOSE_PORT: u16 = 4080;

/// Production goose serve port; probes must never talk to it.
pub fn guard_url(url: &str) -> anyhow::Result<()> {
    let parsed = Url::parse(url)?;
    if parsed.port() == Some(PRODUCTION_GOOSE_PORT) {
        anyhow::bail!("refusing to connect to production goose port {PRODUCTION_GOOSE_PORT}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_production_port() {
        assert!(guard_url("http://127.0.0.1:4080/acp").is_err());
        assert!(guard_url("http://127.0.0.1:4180/acp").is_ok());
    }

    #[test]
    fn refuses_unparseable_url() {
        assert!(guard_url("not a valid url").is_err());
    }
}
