//! The `analytics:` section (docs/design/analytics.md §11): server-side usage analytics
//! kept in `analytics.sqlite`, its retention, the crawl detector's thresholds, the
//! library scan and the opt-in geography lookup.

use super::{
    ConfigError, get_bool, get_float, get_int, get_str, invalid, leftovers, parse_bool, section,
};
use serde_json::{Map, Value, json};

/// The DB-IP "IP to Country Lite" download (CC BY 4.0); `{year}`/`{month}` are filled
/// in with the current (or previous) month.
pub const DEFAULT_GEO_URL: &str =
    "https://download.db-ip.com/free/dbip-country-lite-{year}-{month}.mmdb.gz";

#[derive(Debug, Clone, PartialEq)]
pub struct AnalyticsConfig {
    /// Off: requests pass the accounting layer untouched and nothing is written; what is
    /// stored stays viewable.
    pub enabled: bool,
    /// How often the in-memory window is written to the database.
    pub flush_seconds: u32,
    pub retention: AnalyticsRetention,
    pub crawl: CrawlConfig,
    pub library_scan: LibraryScanConfig,
    pub geo: GeoConfig,
}

/// Days each kind of row is kept. Daily aggregates are kept forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnalyticsRetention {
    /// Raw client IPs (`ip_daily`, `anon_ips`, `auth_failures`, slow requests, a crawl
    /// episode's address).
    pub ip_days: u32,
    /// Hourly traffic and latency rows.
    pub hourly_days: u32,
    pub host_minute_days: u32,
    pub crawl_days: u32,
}

/// When a client IP is reported as crawling the catalog.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CrawlConfig {
    pub window_hours: u32,
    /// Distinct series touched within the window...
    pub min_series: u32,
    /// ...that are at least this share of the catalog's series (0..=1).
    pub min_coverage: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LibraryScanConfig {
    pub interval_hours: u32,
    /// Count the page images inside archives (local libraries only).
    pub page_formats: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeoConfig {
    /// Opt-in: download the country database and look client IPs up in it.
    pub enabled: bool,
    pub url: String,
}

impl Default for AnalyticsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            flush_seconds: 60,
            retention: AnalyticsRetention::default(),
            crawl: CrawlConfig::default(),
            library_scan: LibraryScanConfig::default(),
            geo: GeoConfig::default(),
        }
    }
}

impl Default for AnalyticsRetention {
    fn default() -> Self {
        Self {
            ip_days: 30,
            hourly_days: 90,
            host_minute_days: 7,
            crawl_days: 90,
        }
    }
}

impl Default for CrawlConfig {
    fn default() -> Self {
        Self {
            window_hours: 6,
            min_series: 20,
            min_coverage: 0.25,
        }
    }
}

impl Default for LibraryScanConfig {
    fn default() -> Self {
        Self {
            interval_hours: 6,
            page_formats: true,
        }
    }
}

impl Default for GeoConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: DEFAULT_GEO_URL.into(),
        }
    }
}

/// The keys `analytics.<field>` takes (`config set`, `MOKURO_ANALYTICS_<FIELD>` with
/// dots as underscores).
pub const ANALYTICS_FIELDS: &[&str] = &[
    "enabled",
    "flush_seconds",
    "retention.ip_days",
    "retention.hourly_days",
    "retention.host_minute_days",
    "retention.crawl_days",
    "crawl.window_hours",
    "crawl.min_series",
    "crawl.min_coverage",
    "library_scan.interval_hours",
    "library_scan.page_formats",
    "geo.enabled",
    "geo.url",
];

/// `(min, max)` of each whole-number setting.
const FLUSH_SECONDS: (i64, i64) = (5, 3600);
const RETENTION_DAYS: (i64, i64) = (1, 3650);
const HOST_MINUTE_DAYS: (i64, i64) = (1, 365);
const WINDOW_HOURS: (i64, i64) = (1, 168);
const MIN_SERIES: (i64, i64) = (1, 1_000_000);
const INTERVAL_HOURS: (i64, i64) = (1, 720);

fn ranged(key: &str, v: i64, (lo, hi): (i64, i64)) -> Result<u32, ConfigError> {
    if v < lo || v > hi {
        return Err(invalid(format!(
            "analytics.{key}: must be a whole number from {lo} to {hi}, got {v}"
        )));
    }
    Ok(v as u32)
}

impl AnalyticsConfig {
    /// Parse the section (`None`/`null`: defaults). Returns the keys it did not know,
    /// nested as they came, for the config's `extra`.
    pub(super) fn from_map(
        s: Map<String, Value>,
    ) -> Result<(AnalyticsConfig, Map<String, Value>), ConfigError> {
        let d = AnalyticsConfig::default();
        let sec = "analytics";
        let mut c = AnalyticsConfig {
            enabled: get_bool(&s, sec, "enabled", d.enabled)?,
            flush_seconds: ranged(
                "flush_seconds",
                get_int(&s, sec, "flush_seconds", i64::from(d.flush_seconds))?,
                FLUSH_SECONDS,
            )?,
            ..d.clone()
        };
        let mut extra = Map::new();

        let r = section(&s, "retention")?;
        let rs = "analytics.retention";
        let days = |key: &str, default: u32, bounds| -> Result<u32, ConfigError> {
            ranged(
                &format!("retention.{key}"),
                get_int(&r, rs, key, i64::from(default))?,
                bounds,
            )
        };
        c.retention = AnalyticsRetention {
            ip_days: days("ip_days", d.retention.ip_days, RETENTION_DAYS)?,
            hourly_days: days("hourly_days", d.retention.hourly_days, RETENTION_DAYS)?,
            host_minute_days: days(
                "host_minute_days",
                d.retention.host_minute_days,
                HOST_MINUTE_DAYS,
            )?,
            crawl_days: days("crawl_days", d.retention.crawl_days, RETENTION_DAYS)?,
        };
        let r_extra = leftovers(
            r,
            &["ip_days", "hourly_days", "host_minute_days", "crawl_days"],
        );

        let k = section(&s, "crawl")?;
        let ks = "analytics.crawl";
        c.crawl = CrawlConfig {
            window_hours: ranged(
                "crawl.window_hours",
                get_int(&k, ks, "window_hours", i64::from(d.crawl.window_hours))?,
                WINDOW_HOURS,
            )?,
            min_series: ranged(
                "crawl.min_series",
                get_int(&k, ks, "min_series", i64::from(d.crawl.min_series))?,
                MIN_SERIES,
            )?,
            min_coverage: get_float(&k, ks, "min_coverage", d.crawl.min_coverage)?,
        };
        let k_extra = leftovers(k, &["window_hours", "min_series", "min_coverage"]);

        let l = section(&s, "library_scan")?;
        let ls = "analytics.library_scan";
        c.library_scan = LibraryScanConfig {
            interval_hours: ranged(
                "library_scan.interval_hours",
                get_int(
                    &l,
                    ls,
                    "interval_hours",
                    i64::from(d.library_scan.interval_hours),
                )?,
                INTERVAL_HOURS,
            )?,
            page_formats: get_bool(&l, ls, "page_formats", d.library_scan.page_formats)?,
        };
        let l_extra = leftovers(l, &["interval_hours", "page_formats"]);

        let g = section(&s, "geo")?;
        c.geo = GeoConfig {
            enabled: get_bool(&g, "analytics.geo", "enabled", d.geo.enabled)?,
            url: get_str(&g, "url", &d.geo.url),
        };
        let g_extra = leftovers(g, &["enabled", "url"]);

        c.validate()?;
        let top = leftovers(
            s,
            &[
                "enabled",
                "flush_seconds",
                "retention",
                "crawl",
                "library_scan",
                "geo",
            ],
        );
        extra.extend(top);
        for (name, m) in [
            ("retention", r_extra),
            ("crawl", k_extra),
            ("library_scan", l_extra),
            ("geo", g_extra),
        ] {
            if !m.is_empty() {
                extra.insert(name.into(), Value::Object(m));
            }
        }
        Ok((c, extra))
    }

    /// The checks every way in (file, `config set`, environment, admin editor) passes.
    pub fn validate(&self) -> Result<(), ConfigError> {
        ranged(
            "flush_seconds",
            i64::from(self.flush_seconds),
            FLUSH_SECONDS,
        )?;
        let r = &self.retention;
        ranged("retention.ip_days", i64::from(r.ip_days), RETENTION_DAYS)?;
        ranged(
            "retention.hourly_days",
            i64::from(r.hourly_days),
            RETENTION_DAYS,
        )?;
        ranged(
            "retention.host_minute_days",
            i64::from(r.host_minute_days),
            HOST_MINUTE_DAYS,
        )?;
        ranged(
            "retention.crawl_days",
            i64::from(r.crawl_days),
            RETENTION_DAYS,
        )?;
        ranged(
            "crawl.window_hours",
            i64::from(self.crawl.window_hours),
            WINDOW_HOURS,
        )?;
        ranged(
            "crawl.min_series",
            i64::from(self.crawl.min_series),
            MIN_SERIES,
        )?;
        let cov = self.crawl.min_coverage;
        if !(cov.is_finite() && cov > 0.0 && cov <= 1.0) {
            return Err(invalid(format!(
                "analytics.crawl.min_coverage: must be above 0 and at most 1, got {cov}"
            )));
        }
        ranged(
            "library_scan.interval_hours",
            i64::from(self.library_scan.interval_hours),
            INTERVAL_HOURS,
        )?;
        // HTTPS only: the database decides every visitor's country.
        let url = self.geo.url.trim();
        if !url
            .get(..8)
            .is_some_and(|p| p.eq_ignore_ascii_case("https://"))
        {
            return Err(invalid(format!(
                "analytics.geo.url: must be an https:// URL, got '{url}'"
            )));
        }
        Ok(())
    }

    pub fn to_value(&self) -> Value {
        json!({
            "enabled": self.enabled,
            "flush_seconds": self.flush_seconds,
            "retention": {
                "ip_days": self.retention.ip_days,
                "hourly_days": self.retention.hourly_days,
                "host_minute_days": self.retention.host_minute_days,
                "crawl_days": self.retention.crawl_days,
            },
            "crawl": {
                "window_hours": self.crawl.window_hours,
                "min_series": self.crawl.min_series,
                "min_coverage": self.crawl.min_coverage,
            },
            "library_scan": {
                "interval_hours": self.library_scan.interval_hours,
                "page_formats": self.library_scan.page_formats,
            },
            "geo": {"enabled": self.geo.enabled, "url": self.geo.url},
        })
    }

    /// Set one of [`ANALYTICS_FIELDS`] from its string form. The caller validates the
    /// whole section afterwards.
    pub fn set_field(&mut self, field: &str, value: &str) -> Result<(), ConfigError> {
        let key = format!("analytics.{field}");
        let b = || {
            parse_bool(value)
                .ok_or_else(|| invalid(format!("{key}: Invalid boolean value: {value}")))
        };
        let i = || {
            value
                .trim()
                .parse::<i64>()
                .map_err(|_| invalid(format!("{key}: expected a whole number, got '{value}'")))
        };
        let n = |bounds| -> Result<u32, ConfigError> { ranged(field, i()?, bounds) };
        match field {
            "enabled" => self.enabled = b()?,
            "flush_seconds" => self.flush_seconds = n(FLUSH_SECONDS)?,
            "retention.ip_days" => self.retention.ip_days = n(RETENTION_DAYS)?,
            "retention.hourly_days" => self.retention.hourly_days = n(RETENTION_DAYS)?,
            "retention.host_minute_days" => self.retention.host_minute_days = n(HOST_MINUTE_DAYS)?,
            "retention.crawl_days" => self.retention.crawl_days = n(RETENTION_DAYS)?,
            "crawl.window_hours" => self.crawl.window_hours = n(WINDOW_HOURS)?,
            "crawl.min_series" => self.crawl.min_series = n(MIN_SERIES)?,
            "crawl.min_coverage" => {
                self.crawl.min_coverage = value
                    .trim()
                    .parse()
                    .map_err(|_| invalid(format!("{key}: expected a number, got '{value}'")))?
            }
            "library_scan.interval_hours" => self.library_scan.interval_hours = n(INTERVAL_HOURS)?,
            "library_scan.page_formats" => self.library_scan.page_formats = b()?,
            "geo.enabled" => self.geo.enabled = b()?,
            "geo.url" => self.geo.url = value.trim().to_string(),
            _ => {
                return Err(ConfigError::UnknownKey(format!(
                    "Unknown field '{field}' in section 'analytics'"
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::Config;
    use super::*;

    fn yaml(s: &str) -> Value {
        serde_yaml_ng::from_str(s).unwrap()
    }

    #[test]
    fn defaults_match_the_design() {
        let c = Config::from_value(&Value::Null).unwrap();
        let a = &c.analytics;
        assert!(a.enabled);
        assert_eq!(a.flush_seconds, 60);
        assert_eq!(
            (
                a.retention.ip_days,
                a.retention.hourly_days,
                a.retention.host_minute_days,
                a.retention.crawl_days
            ),
            (30, 90, 7, 90)
        );
        assert_eq!((a.crawl.window_hours, a.crawl.min_series), (6, 20));
        assert_eq!(a.crawl.min_coverage, 0.25);
        assert_eq!(
            a.library_scan,
            LibraryScanConfig {
                interval_hours: 6,
                page_formats: true
            }
        );
        assert!(!a.geo.enabled);
        assert_eq!(a.geo.url, DEFAULT_GEO_URL);
        // Not written while it is the default (config files stay as they were).
        assert!(c.to_value().get("analytics").is_none());
    }

    #[test]
    fn parses_validates_and_round_trips() {
        let c = Config::from_value(&yaml(
            "analytics:\n  enabled: false\n  flush_seconds: 30\n  retention: {ip_days: 7, hourly_days: 14, future: 1}\n  crawl: {min_series: 5, min_coverage: 0.5}\n  geo: {enabled: true}\n  knob: x\n",
        ))
        .unwrap();
        assert!(!c.analytics.enabled);
        assert_eq!(c.analytics.flush_seconds, 30);
        assert_eq!(c.analytics.retention.ip_days, 7);
        assert_eq!(c.analytics.retention.crawl_days, 90);
        assert_eq!(c.analytics.crawl.min_series, 5);
        assert!(c.analytics.geo.enabled);
        let v = c.to_value();
        assert_eq!(v["analytics"]["retention"]["hourly_days"], 14);
        assert_eq!(
            v["analytics"]["retention"]["future"], 1,
            "unknown keys kept"
        );
        assert_eq!(v["analytics"]["knob"], "x");
        let again = Config::from_value(&v).unwrap();
        assert_eq!(again.analytics, c.analytics);
        assert_eq!(again.to_value(), v);

        for bad in [
            "analytics: {flush_seconds: 1}",
            "analytics: {retention: {ip_days: 0}}",
            "analytics: {crawl: {min_coverage: 1.5}}",
            "analytics: {crawl: {min_coverage: 0}}",
            "analytics: {crawl: {window_hours: 500}}",
            "analytics: {library_scan: {interval_hours: 0}}",
            "analytics: {geo: {url: 'ftp://example.com/x'}}",
            "analytics: {geo: {url: 'http://example.com/x'}}",
            "analytics: {enabled: maybe}",
            "analytics: 3",
        ] {
            assert!(Config::from_value(&yaml(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn dotted_keys_and_environment() {
        let mut c = Config::default();
        c.set_by_dotted_key("analytics.retention.ip_days", "14")
            .unwrap();
        c.set_by_dotted_key("analytics.crawl.min_coverage", "0.4")
            .unwrap();
        assert_eq!(c.analytics.retention.ip_days, 14);
        assert_eq!(c.analytics.crawl.min_coverage, 0.4);
        assert!(
            c.set_by_dotted_key("analytics.crawl.min_coverage", "2")
                .is_err()
        );
        assert!(c.set_by_dotted_key("analytics.nope", "1").is_err());
        c.apply_env_overrides(|k| match k {
            "MOKURO_ANALYTICS_ENABLED" => Some("false".into()),
            "MOKURO_ANALYTICS_FLUSH_SECONDS" => Some("15".into()),
            "MOKURO_ANALYTICS_RETENTION_HOURLY_DAYS" => Some("30".into()),
            "MOKURO_ANALYTICS_GEO_ENABLED" => Some("yes".into()),
            _ => None,
        })
        .unwrap();
        assert!(!c.analytics.enabled);
        assert_eq!(c.analytics.flush_seconds, 15);
        assert_eq!(c.analytics.retention.hourly_days, 30);
        assert!(c.analytics.geo.enabled);
        assert!(
            c.apply_env_overrides(|k| (k == "MOKURO_ANALYTICS_FLUSH_SECONDS").then(|| "0".into()))
                .is_err()
        );
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let mut c = Config::default();
        c.analytics.retention.ip_days = 10;
        c.analytics.geo.enabled = true;
        super::super::save_config(&c, Some(&path)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("analytics:"), "{text}");
        let back = super::super::load_config(Some(&path)).unwrap();
        assert_eq!(back.analytics, c.analytics);
    }
}
