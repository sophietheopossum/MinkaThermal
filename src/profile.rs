//! Chassis profile, the Duo constants and the config key (§4).
//!
//! The rules (episodes, verdicts, references) live elsewhere; this module only says which
//! machine this is and which settings the numbers were measured under.

use crate::sensors::{ConfigRead, StaticInfo};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// `thermal.chassis` in ~/.config/minka-settings.json.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChassisSetting {
    Duo,
    Generic,
    // serde's catch-all has to be the last variant.
    #[default]
    #[serde(other)]
    Auto,
}

impl ChassisSetting {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChassisSetting::Auto => "auto",
            ChassisSetting::Duo => "duo",
            ChassisSetting::Generic => "generic",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Chassis {
    Duo,
    #[default]
    #[serde(other)]
    Generic,
}

impl Chassis {
    pub fn as_str(&self) -> &'static str {
        match self {
            Chassis::Duo => "duo",
            Chassis::Generic => "generic",
        }
    }
}

/// Reads `thermal.chassis` from the settings JSON. Anything missing or malformed is `auto`,
/// matching MinkaShell's reset-on-malformed rule for the same file.
pub fn parse_chassis_setting(json: &str) -> ChassisSetting {
    let value: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
    match value.pointer("/thermal/chassis").and_then(|v| v.as_str()) {
        Some("duo") => ChassisSetting::Duo,
        Some("generic") => ChassisSetting::Generic,
        _ => ChassisSetting::Auto,
    }
}

pub fn read_chassis_setting(settings: Option<&Path>) -> ChassisSetting {
    settings
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map_or(ChassisSetting::Auto, |text| parse_chassis_setting(&text))
}

/// The UX482E family by DMI, as MinkaMon's `machine_is_zenbook` recognises it: the same chassis
/// and fan on any UX482E, with or without the MX450.
pub fn is_ux482(info: &StaticInfo) -> bool {
    info.sys_vendor == "ASUSTeK COMPUTER INC."
        && (info.board_name.starts_with("UX482E") || info.product_name.contains("UX482"))
}

pub fn resolve_chassis(setting: ChassisSetting, info: &StaticInfo) -> Chassis {
    match setting {
        ChassisSetting::Duo => Chassis::Duo,
        ChassisSetting::Generic => Chassis::Generic,
        ChassisSetting::Auto if is_ux482(info) => Chassis::Duo,
        ChassisSetting::Auto => Chassis::Generic,
    }
}

/// Chassis constants for any UX482E, whatever the CPU (§4).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DuoConstants {
    /// The automatic plateau of asus `fan1_input`.
    pub fan_auto_min: u32,
    pub fan_auto_max: u32,
    /// Forced maximum, measured on the replacement fan of 29/9/2026.
    pub fan_forced_max: u32,
    /// EC fan stages.
    pub ec_stages: [u32; 3],
    /// A plateau settles this long after onset, once the fan has reached `settle_fan`.
    pub settle_s: f64,
    pub settle_fan: u32,
    /// If the fan is still below `settle_fan` here, settle anyway and flag `fan_low`.
    pub settle_deadline_s: f64,
    /// The fan-slow line is R_fan minus this.
    pub fan_slow_margin: u32,
    /// All-core maximum on the i7-1165G7; other CPUs learn theirs (§6.3).
    pub f_allcore_max_i7_1165g7: u32,
}

pub const DUO: DuoConstants = DuoConstants {
    fan_auto_min: 6800,
    fan_auto_max: 7000,
    fan_forced_max: 7300,
    ec_stages: [5000, 6100, 6900],
    settle_s: 75.0,
    settle_fan: 6500,
    settle_deadline_s: 120.0,
    fan_slow_margin: 400,
    f_allcore_max_i7_1165g7: 4100,
};

/// `f_allcore_max` when it is known up front; None means learn it (§6.3).
pub fn f_allcore_max(chassis: Chassis, cpu_model: &str) -> Option<u32> {
    (chassis == Chassis::Duo && cpu_model.contains("i7-1165G7"))
        .then_some(DUO.f_allcore_max_i7_1165g7)
}

/// The Duo seed of §6.1: 20/11/2026 16:50 UTC, just cleaned, burn.c × 8, recomputed by
/// timestamp. The post-load values are provisional until her first check re-measures them at 4 Hz.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Seed {
    /// "20/11/2026", for provenance labels.
    pub date_label: &'static str,
    pub board: &'static str,
    pub t_ss: f64,
    /// Fraction, not percent: 0.0093 is 0.93 %.
    pub duty: f64,
    pub fan: u32,
    /// Display only: a different generator from the check's.
    pub f: u32,
    pub t_end: f64,
    pub t_hs: f64,
    pub d_jc: f64,
    pub floor: f64,
    pub t_cool_s: f64,
    pub t_fan5000_s: f64,
    pub t_first_throttle_s: f64,
    pub t_fan6500_s: f64,
    pub duty_early: f64,
    /// Median coretemp over the idle phase before the load.
    pub t_idle: f64,
    /// Display only; E2 uses the learned p90.
    pub light_duty: f64,
    /// Million rounds of the C burn: not comparable with generator v1.
    pub work_m_rounds: u64,
}

pub const DUO_SEED: Seed = Seed {
    date_label: "20/11/2026",
    board: "UX482EG",
    t_ss: 86.0,
    duty: 0.0093,
    fan: 6800,
    f: 3899,
    t_end: 86.0,
    t_hs: 56.0,
    d_jc: 30.0,
    floor: 49.0,
    t_cool_s: 14.9,
    t_fan5000_s: 29.7,
    t_first_throttle_s: 4.2,
    t_fan6500_s: 61.6,
    duty_early: 0.37,
    t_idle: 62.0,
    light_duty: 0.0011,
    work_m_rounds: 340_255,
};

/// The settings a reference was measured under (§4). Evidence counts only under a key that has
/// a reference; a key change closes the open episode and window.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConfigKey {
    pub ac: bool,
    pub profile: Option<String>,
    pub epp: Option<String>,
    /// MMIO PL1 and PL2, rounded to whole watts.
    pub pl1_w: Option<u32>,
    pub pl2_w: Option<u32>,
    pub tcc_offset: Option<i64>,
    pub no_turbo: Option<bool>,
    pub max_perf_pct: Option<u32>,
    pub lid_open: Option<bool>,
}

fn watts(uw: Option<u64>) -> Option<u32> {
    uw.map(|v| ((v + 500_000) / 1_000_000) as u32)
}

impl ConfigKey {
    /// An unknown AC state counts as AC: that is a desktop without a battery.
    pub fn new(ac: Option<bool>, cfg: &ConfigRead) -> ConfigKey {
        ConfigKey {
            ac: ac.unwrap_or(true),
            profile: cfg.platform_profile.clone(),
            epp: cfg.epp.clone(),
            pl1_w: watts(cfg.pl1_uw),
            pl2_w: watts(cfg.pl2_uw),
            tcc_offset: cfg.tcc_offset,
            no_turbo: cfg.no_turbo,
            max_perf_pct: cfg.max_perf_pct,
            lid_open: cfg.lid_open,
        }
    }

    /// `ac|performance|performance|28|35|8|0|100|open`; "-" for a signal this machine lacks.
    pub fn id(&self) -> String {
        fn opt<T: ToString>(v: &Option<T>) -> String {
            v.as_ref().map_or("-".to_owned(), T::to_string)
        }
        [
            if self.ac { "ac" } else { "bat" }.to_owned(),
            opt(&self.profile),
            opt(&self.epp),
            opt(&self.pl1_w),
            opt(&self.pl2_w),
            opt(&self.tcc_offset),
            opt(&self.no_turbo.map(u8::from)),
            opt(&self.max_perf_pct),
            match self.lid_open {
                Some(true) => "open",
                Some(false) => "closed",
                None => "-",
            }
            .to_owned(),
        ]
        .join("|")
    }

    /// The key back from its `id()`, for records that store only the id (`replay`). None for a
    /// string `id()` cannot have produced.
    pub fn parse_id(id: &str) -> Option<ConfigKey> {
        let f: Vec<&str> = id.split('|').collect();
        let [ac, profile, epp, pl1, pl2, tcc, no_turbo, max_perf, lid] = f.as_slice() else {
            return None;
        };
        fn opt<T: std::str::FromStr>(v: &str) -> Option<Option<T>> {
            if v == "-" {
                Some(None)
            } else {
                v.parse().ok().map(Some)
            }
        }
        let text = |v: &str| (v != "-").then(|| v.to_owned());
        Some(ConfigKey {
            ac: match *ac {
                "ac" => true,
                "bat" => false,
                _ => return None,
            },
            profile: text(profile),
            epp: text(epp),
            pl1_w: opt(pl1)?,
            pl2_w: opt(pl2)?,
            tcc_offset: opt(tcc)?,
            no_turbo: opt::<u8>(no_turbo)?.map(|v| v != 0),
            max_perf_pct: opt(max_perf)?,
            lid_open: match *lid {
                "open" => Some(true),
                "closed" => Some(false),
                "-" => None,
                _ => return None,
            },
        })
    }

    /// `AC · performance · PL1 28 W · PL2 35 W · TCC 92 °C · lid open`. EPP, no_turbo and
    /// max_perf_pct appear only when they differ from the usual (EPP equal to the profile, turbo
    /// on, 100 %), which keeps her normal key short.
    pub fn label(&self, tcc_c: Option<f64>) -> String {
        let mut parts = vec![if self.ac { "AC" } else { "battery" }.to_owned()];
        if let Some(p) = &self.profile {
            parts.push(p.clone());
        }
        if let Some(e) = &self.epp
            && self.profile.as_ref() != Some(e)
        {
            parts.push(format!("EPP {e}"));
        }
        if let Some(w) = self.pl1_w {
            parts.push(format!("PL1 {w} W"));
        }
        if let Some(w) = self.pl2_w {
            parts.push(format!("PL2 {w} W"));
        }
        match (tcc_c, self.tcc_offset) {
            (Some(t), _) => parts.push(format!("TCC {t:.0} °C")),
            (None, Some(o)) => parts.push(format!("TCC offset {o}")),
            (None, None) => {}
        }
        if self.no_turbo == Some(true) {
            parts.push("no turbo".to_owned());
        }
        if let Some(p) = self.max_perf_pct.filter(|&p| p != 100) {
            parts.push(format!("max perf {p} %"));
        }
        match self.lid_open {
            Some(true) => parts.push("lid open".to_owned()),
            Some(false) => parts.push("lid closed".to_owned()),
            None => {}
        }
        parts.join(" · ")
    }

    /// Her normal state, verified live on 20/11/2026: the key the Duo seed applies to (§4).
    pub fn is_seed_key(&self) -> bool {
        self.ac
            && self.profile.as_deref() == Some("performance")
            && self.epp.as_deref() == Some("performance")
            && self.pl1_w.is_some_and(|w| (26..=30).contains(&w))
            && self.pl2_w.is_some_and(|w| w >= 33)
            && self.tcc_offset == Some(8)
            && self.no_turbo == Some(false)
            && self.max_perf_pct == Some(100)
            && self.lid_open == Some(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn her_config() -> ConfigRead {
        ConfigRead {
            platform_profile: Some("performance".into()),
            epp: Some("performance".into()),
            pl_domain: Some("intel-rapl-mmio:0".into()),
            pl1_uw: Some(28_000_000),
            pl2_uw: Some(35_000_000),
            pl1_max_uw: Some(28_000_000),
            tcc_offset: Some(8),
            no_turbo: Some(false),
            max_perf_pct: Some(100),
            cooling_max: Some(0),
            dgpu_runtime: Some("suspended".into()),
            thermald: false,
            lid_open: Some(true),
            ..ConfigRead::default()
        }
    }

    #[test]
    fn her_key() {
        let k = ConfigKey::new(Some(true), &her_config());
        assert_eq!(k.id(), "ac|performance|performance|28|35|8|0|100|open");
        assert_eq!(
            k.label(Some(92.0)),
            "AC · performance · PL1 28 W · PL2 35 W · TCC 92 °C · lid open"
        );
        assert!(k.is_seed_key());
    }

    #[test]
    fn other_keys() {
        let mut cfg = her_config();
        cfg.epp = Some("balance_power".into());
        cfg.pl1_uw = Some(15_400_000);
        cfg.no_turbo = Some(true);
        cfg.lid_open = Some(false);
        let k = ConfigKey::new(Some(false), &cfg);
        assert_eq!(k.id(), "bat|performance|balance_power|15|35|8|1|100|closed");
        assert_eq!(
            k.label(None),
            "battery · performance · EPP balance_power · PL1 15 W · PL2 35 W · TCC offset 8 · no turbo · lid closed"
        );
        assert!(!k.is_seed_key());

        let k = ConfigKey::new(None, &ConfigRead::default());
        assert_eq!(k.id(), "ac|-|-|-|-|-|-|-|-");
        assert!(k.ac, "no AC reading is a desktop");
        assert_eq!(ConfigKey::parse_id(&k.id()), Some(k));

        let k = ConfigKey::new(Some(true), &her_config());
        assert_eq!(ConfigKey::parse_id(&k.id()).as_ref(), Some(&k));
        assert!(ConfigKey::parse_id(&k.id()).unwrap().is_seed_key());
        for bad in ["", "ac|performance", "dc|-|-|-|-|-|-|-|-", "ac|-|-|x|-|-|-|-|-"] {
            assert_eq!(ConfigKey::parse_id(bad), None, "{bad}");
        }

        // The seed's PL1 range is 26-30 W, PL2 at least 33 W.
        let mut cfg = her_config();
        cfg.pl1_uw = Some(30_400_000);
        assert!(ConfigKey::new(Some(true), &cfg).is_seed_key());
        cfg.pl2_uw = Some(32_000_000);
        assert!(!ConfigKey::new(Some(true), &cfg).is_seed_key());
    }

    #[test]
    fn chassis() {
        assert_eq!(
            parse_chassis_setting(r#"{"thermal":{"chassis":"duo"}}"#),
            ChassisSetting::Duo
        );
        assert_eq!(
            parse_chassis_setting(r#"{"shell":{"layout":"auto"},"thermal":{"chassis":"generic"}}"#),
            ChassisSetting::Generic
        );
        assert_eq!(
            parse_chassis_setting(r#"{"thermal":{"chassis":"tower"}}"#),
            ChassisSetting::Auto
        );
        assert_eq!(parse_chassis_setting("not json"), ChassisSetting::Auto);
        assert_eq!(read_chassis_setting(None), ChassisSetting::Auto);

        let mut info = StaticInfo {
            sys_vendor: "ASUSTeK COMPUTER INC.".into(),
            board_name: "UX482EG".into(),
            product_name: "ZenBook UX482EG_UX482EG".into(),
            ..StaticInfo::default()
        };
        assert_eq!(resolve_chassis(ChassisSetting::Auto, &info), Chassis::Duo);
        assert_eq!(
            resolve_chassis(ChassisSetting::Generic, &info),
            Chassis::Generic
        );
        info.board_name = "UX482EA".into();
        assert!(is_ux482(&info));
        info.board_name = "UX425EA".into();
        info.product_name = "ZenBook UX425EA".into();
        assert_eq!(
            resolve_chassis(ChassisSetting::Auto, &info),
            Chassis::Generic
        );
        assert_eq!(resolve_chassis(ChassisSetting::Duo, &info), Chassis::Duo);
        info.sys_vendor = "LENOVO".into();
        info.board_name = "UX482EG".into();
        assert!(!is_ux482(&info));

        assert_eq!(
            f_allcore_max(Chassis::Duo, "11th Gen Intel(R) Core(TM) i7-1165G7"),
            Some(4100)
        );
        assert_eq!(
            f_allcore_max(Chassis::Duo, "11th Gen Intel(R) Core(TM) i5-1135G7"),
            None
        );
        assert_eq!(
            f_allcore_max(Chassis::Generic, "11th Gen Intel(R) Core(TM) i7-1165G7"),
            None
        );
    }
}
