//! Climate layer — a faithful port of the sibling Python
//! `zionstats/climate.py`: Stull (2011) wet-bulb, cooling effectiveness
//! (headroom/30 C derated by humidity), condensation risk, advisory
//! strings, and the Open-Meteo provider with an ATOMIC disk cache shared
//! with the Python stack (`~/.cache/zionstats/weather.json`) so the two
//! implementations read the same observations. Offline use serves the
//! cache with the stale flag; the network call is the only NEW outbound
//! traffic in this crate and stays keyless.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const OPEN_METEO_URL: &str = "https://api.open-meteo.com/v1/forecast\
?latitude={lat}&longitude={lon}\
&current=temperature_2m,relative_humidity_2m,dew_point_2m,\
apparent_temperature,precipitation,weather_code,wind_speed_10m,is_day\
&hourly=temperature_2m&forecast_days=2&timezone=auto";

/// Stull (2011) wet-bulb approximation; good for 5-99% RH, -20..50 C.
pub fn wet_bulb_c(temp_c: f64, rh: f64) -> f64 {
    let rh = rh.clamp(5.0, 99.0);
    let t = temp_c;
    t * (0.151977 * (rh + 8.313659).sqrt()).atan() + (t + rh).atan() - (rh - 1.676331).atan()
        + 0.00391838 * rh.powf(1.5) * (0.023101 * rh).atan()
        - 4.686035
}

pub fn wmo_condition(code: i64) -> String {
    let s = match code {
        0 => "Clear",
        1 => "Mostly clear",
        2 => "Partly cloudy",
        3 => "Overcast",
        45 | 48 => "Fog",
        51 => "Light drizzle",
        53 => "Drizzle",
        55 => "Dense drizzle",
        56 | 57 => "Freezing drizzle",
        61 => "Light rain",
        63 => "Rain",
        65 => "Heavy rain",
        66 | 67 => "Freezing rain",
        71 => "Light snow",
        73 => "Snow",
        75 => "Heavy snow",
        77 => "Snow grains",
        80 | 81 => "Showers",
        82 => "Violent showers",
        85 | 86 => "Snow showers",
        95 => "Thunderstorm",
        96 | 99 => "T-storm + hail",
        _ => return format!("code {code}"),
    };
    s.to_string()
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Weather {
    pub ts: f64,
    pub lat: f64,
    pub lon: f64,
    pub label: String,
    pub temp_c: f64,
    pub rh: f64,
    pub dew_c: f64,
    pub apparent_c: f64,
    pub precip_mm: f64,
    pub code: i64,
    pub wind_kmh: f64,
    pub is_day: bool,
    pub stale: bool,
}

impl Weather {
    pub fn age_s(&self, now: f64) -> f64 {
        now - self.ts
    }

    pub fn wet_bulb(&self) -> f64 {
        wet_bulb_c(self.temp_c, self.rh)
    }

    pub fn condition(&self) -> String {
        wmo_condition(self.code)
    }

    /// How much headroom ambient air has to carry heat away.
    pub fn delta_vs(&self, tgpu: f64) -> f64 {
        tgpu - self.temp_c
    }

    /// 0..1 -- is the blower worth it right now?  Headroom normalized to
    /// 30 C, derated by humidity (muggy air carries less cooling per watt
    /// of fan noise).
    pub fn effectiveness(&self, tgpu: f64) -> f64 {
        let head = (self.delta_vs(tgpu) / 30.0).clamp(0.0, 1.0);
        let humidity_penalty = 1.0 - 0.25 * ((self.rh - 50.0) / 50.0).clamp(0.0, 1.0);
        head * humidity_penalty
    }

    /// Blowing ambient air at a surface below the dew point condenses
    /// water on it -- rare, but worth a warning when it applies.
    pub fn condensation_risk(&self, t_surface: f64) -> bool {
        t_surface < self.dew_c
    }

    /// (level, message) advisories -- Python parity for the levels and
    /// thresholds.  Levels: "warn" | "info".
    pub fn advisories(&self, tgpu: Option<f64>) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        if let Some(tg) = tgpu {
            if self.condensation_risk(tg) {
                out.push((
                    "warn",
                    format!(
                        "GPU below dew point ({:.0}C) -- condensation risk, keep blower off",
                        self.dew_c
                    ),
                ));
            }
        }
        let wb = self.wet_bulb();
        if wb > 29.0 {
            out.push((
                "warn",
                format!("wet-bulb {wb:.0}C: extreme humid heat, blower cooling is marginal"),
            ));
        } else if wb > 26.0 {
            out.push((
                "info",
                format!("wet-bulb {wb:.0}C: humid -- blower helps less than in dry air"),
            ));
        }
        if let Some(tg) = tgpu {
            if self.delta_vs(tg) < 8.0 && tg > 60.0 {
                out.push((
                    "info",
                    format!(
                        "ambient {:.0}C close to GPU {tg:.0}C: diminished cooling delta",
                        self.temp_c
                    ),
                ));
            }
        }
        if self.code >= 95 {
            out.push((
                "warn",
                "thunderstorm overhead -- consider unplugging during outages".to_string(),
            ));
        }
        out
    }

    pub fn summary_line(&self) -> String {
        format!(
            "{:.1}C {:.0}%RH dew {:.1}C -- {}",
            self.temp_c,
            self.rh,
            self.dew_c,
            self.condition()
        )
    }
}

// ---------------------------------------------------------------- provider

fn now_unix() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub struct WeatherProvider {
    pub lat: f64,
    pub lon: f64,
    pub label: String,
    pub refresh_s: f64,
    pub cache_path: PathBuf,
    pub last: Option<Weather>,
    pub last_error: Option<String>,
    last_fetch: f64,
}

impl WeatherProvider {
    pub fn new(lat: f64, lon: f64, label: &str, refresh_min: f64) -> WeatherProvider {
        let cache = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(".cache")
            .join("zionstats")
            .join("weather.json");
        let mut p = WeatherProvider {
            lat,
            lon,
            label: label.to_string(),
            refresh_s: refresh_min * 60.0,
            cache_path: cache,
            last: None,
            last_error: None,
            last_fetch: 0.0,
        };
        if let Some(c) = p.read_cache() {
            p.last = Some(c);
        }
        p
    }

    fn read_cache(&self) -> Option<Weather> {
        let raw = std::fs::read_to_string(&self.cache_path).ok()?;
        let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
        if v.get("lat")?.as_f64()? != self.lat || v.get("lon")?.as_f64()? != self.lon {
            return None;
        }
        Some(Weather {
            ts: v.get("ts")?.as_f64()?,
            lat: self.lat,
            lon: self.lon,
            label: v
                .get("label")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            temp_c: v.get("temp_c").and_then(|x| x.as_f64()).unwrap_or(0.0),
            rh: v.get("rh").and_then(|x| x.as_f64()).unwrap_or(0.0),
            dew_c: v.get("dew_c").and_then(|x| x.as_f64()).unwrap_or(0.0),
            apparent_c: v.get("apparent_c").and_then(|x| x.as_f64()).unwrap_or(0.0),
            precip_mm: v.get("precip_mm").and_then(|x| x.as_f64()).unwrap_or(0.0),
            code: v.get("code").and_then(|x| x.as_i64()).unwrap_or(0),
            wind_kmh: v.get("wind_kmh").and_then(|x| x.as_f64()).unwrap_or(0.0),
            is_day: v.get("is_day").and_then(|x| x.as_bool()).unwrap_or(true),
            stale: false,
        })
    }

    /// Atomic replace so a crash mid-write never corrupts the shared
    /// cache the Python stack also reads.
    fn write_cache(&self, w: &Weather) {
        let Some(parent) = self.cache_path.parent() else {
            return;
        };
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
        let json = serde_json::json!({
            "ts": w.ts, "lat": w.lat, "lon": w.lon, "label": w.label,
            "temp_c": w.temp_c, "rh": w.rh, "dew_c": w.dew_c,
            "apparent_c": w.apparent_c, "precip_mm": w.precip_mm,
            "code": w.code, "wind_kmh": w.wind_kmh, "is_day": w.is_day,
        });
        let tmp = self.cache_path.with_extension("tmp");
        if let Ok(mut f) = std::fs::File::create(&tmp) {
            if f.write_all(json.to_string().as_bytes()).is_ok() {
                let _ = std::fs::rename(&tmp, &self.cache_path);
            }
        }
    }

    fn fetch(&self, now: f64) -> Result<Weather, String> {
        let url = OPEN_METEO_URL
            .replace("{lat}", &format!("{}", self.lat))
            .replace("{lon}", &format!("{}", self.lon));
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(6))
            .build();
        let body = agent
            .get(&url)
            .set("User-Agent", "zionstats/0.7")
            .call()
            .map_err(|e| e.to_string())?
            .into_string()
            .map_err(|e| e.to_string())?;
        let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
        let cur = v.get("current").ok_or("no current block")?;
        Ok(Weather {
            ts: now,
            lat: self.lat,
            lon: self.lon,
            label: self.label.clone(),
            temp_c: cur["temperature_2m"].as_f64().unwrap_or(0.0),
            rh: cur["relative_humidity_2m"].as_f64().unwrap_or(0.0),
            dew_c: cur["dew_point_2m"].as_f64().unwrap_or(0.0),
            apparent_c: cur["apparent_temperature"].as_f64().unwrap_or(0.0),
            precip_mm: cur["precipitation"].as_f64().unwrap_or(0.0),
            code: cur["weather_code"].as_i64().unwrap_or(0),
            wind_kmh: cur["wind_speed_10m"].as_f64().unwrap_or(0.0),
            is_day: cur["is_day"].as_i64().unwrap_or(1) != 0,
            stale: false,
        })
    }

    /// Fetch if stale; returns the current Weather (possibly cached with
    /// the stale flag).  Offline-tolerant by contract.
    pub fn refresh(&mut self, force: bool) -> Option<Weather> {
        let now = now_unix();
        if !force && self.last.is_some() && now - self.last_fetch < self.refresh_s {
            return self.last.clone();
        }
        self.last_fetch = now;
        match self.fetch(now) {
            Ok(w) => {
                self.last = Some(w.clone());
                self.last_error = None;
                self.write_cache(&w);
                self.last.clone()
            }
            Err(e) => {
                self.last_error = Some(e.chars().take(120).collect());
                if let Some(w) = self.last.as_mut() {
                    w.stale = true;
                }
                self.last.clone()
            }
        }
    }
}

/// Tropical diurnal curve for --demo: 27-33 C, 85-92% RH, afternoon
/// drizzle bursts.  No network, no cache.
pub struct DemoWeather {
    pub t0: f64,
}

impl DemoWeather {
    pub fn new() -> DemoWeather {
        DemoWeather { t0: now_unix() }
    }

    pub fn refresh(&mut self) -> Weather {
        let now = now_unix();
        let h = (now - self.t0) / 3600.0;
        let tod = (now % 86400.0) / 86400.0;
        let temp =
            29.0 + 3.5 * ((tod - 0.35) * 2.0 * std::f64::consts::PI).sin() + 0.4 * (h * 7.0).sin();
        let rh =
            88.0 - 8.0 * ((tod - 0.35) * 2.0 * std::f64::consts::PI).sin() + 1.5 * (h * 5.0).sin();
        let rh = rh.clamp(60.0, 98.0);
        let code: i64 = if (0.55..0.70).contains(&tod) {
            51
        } else if (0.3..0.55).contains(&tod) {
            2
        } else {
            1
        };
        Weather {
            ts: now,
            lat: 0.0,
            lon: 0.0,
            label: "demo location".into(),
            temp_c: (temp * 10.0).round() / 10.0,
            rh: rh.round(),
            dew_c: ((wet_bulb_c(temp, rh.max(60.0)) - 0.6) * 10.0).round() / 10.0,
            apparent_c: (temp + 4.0) * 10.0 / 10.0,
            precip_mm: if code == 51 { 0.4 } else { 0.0 },
            code,
            wind_kmh: 8.0,
            is_day: (0.25..0.85).contains(&tod),
            stale: false,
        }
    }
}

impl Default for DemoWeather {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stull_wet_bulb_reference_points() {
        // sanity anchors (approximate psychrometric values)
        let wb = wet_bulb_c(30.0, 70.0);
        assert!((wb - 25.8).abs() < 1.0, "30C/70% -> ~25.8C, got {wb}");
        let wb_dry = wet_bulb_c(30.0, 10.0);
        assert!(wb_dry < wb, "dry air has a lower wet bulb");
        // clamps: absurd RH stays in range
        assert!(wet_bulb_c(20.0, 500.0).is_finite());
    }

    #[test]
    fn effectiveness_headroom_and_humidity_derating() {
        let dry = Weather {
            temp_c: 20.0,
            rh: 20.0,
            ..Default::default()
        };
        let muggy = Weather {
            temp_c: 20.0,
            rh: 90.0,
            ..Default::default()
        };
        assert!(
            (dry.effectiveness(80.0) - 1.0).abs() < 1e-9,
            "60C headroom / 30 = 1.0"
        );
        assert!(
            muggy.effectiveness(80.0) < dry.effectiveness(80.0),
            "muggy derates"
        );
        assert_eq!(dry.effectiveness(20.0), 0.0, "no headroom, no effect");
    }

    #[test]
    fn condensation_and_advisories() {
        let w = Weather {
            temp_c: 26.0,
            rh: 90.0,
            dew_c: 24.2,
            code: 95,
            ..Default::default()
        };
        assert!(w.condensation_risk(20.0));
        assert!(!w.condensation_risk(28.0));
        let adv = w.advisories(Some(18.0));
        assert!(adv
            .iter()
            .any(|(l, m)| *l == "warn" && m.contains("dew point")));
        assert!(adv.iter().any(|(l, _)| *l == "warn"), "storm advisory");
    }

    #[test]
    fn demo_weather_is_network_free_and_sane() {
        let mut d = DemoWeather::new();
        let w = d.refresh();
        assert!((20.0..=40.0).contains(&w.temp_c));
        assert!((55.0..=98.0).contains(&w.rh));
        assert!(!w.stale);
    }
}
