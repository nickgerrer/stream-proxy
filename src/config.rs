use std::time::Duration;

/// Runtime configuration loaded from environment variables
pub struct Config {
    pub port: u16,
    pub transient_cooldown: Duration,
    pub hard_cooldown: Duration,
    pub max_failovers: u32,
    pub max_same_url_retries: u32,
    pub retry_delay: Duration,
    /// URL of the Transmitarr Laravel app (for event push). `None` disables event push.
    pub transmitarr_url: Option<String>,
    /// How often (in seconds) to flush buffered events to Transmitarr.
    pub metrics_flush_interval_secs: u64,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            port: env_or("PORT", 8888),
            transient_cooldown: Duration::from_secs(env_or("TRANSIENT_COOLDOWN_SECS", 60)),
            hard_cooldown: Duration::from_secs(env_or("HARD_COOLDOWN_SECS", 1800)),
            max_failovers: env_or("MAX_FAILOVERS", 10),
            max_same_url_retries: env_or("MAX_SAME_URL_RETRIES", 3),
            retry_delay: Duration::from_secs(env_or("RETRY_DELAY_SECS", 3)),
            transmitarr_url: std::env::var("TRANSMITARR_URL").ok().filter(|s| !s.is_empty()),
            metrics_flush_interval_secs: env_or("METRICS_FLUSH_SECS", 2),
        }
    }
}

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
