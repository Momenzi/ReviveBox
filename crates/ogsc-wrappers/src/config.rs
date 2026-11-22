//! Deterministic CRLF configuration file generators for compatibility wrappers and emulators.

use std::collections::BTreeMap;

use ogsc_recipes::LegacyOsProfile;

/// Generates `dgVoodoo.conf` with clean Windows 10/11 defaults merged with recipe `overrides`.
pub fn render_dgvoodoo_conf(overrides: &BTreeMap<String, BTreeMap<String, String>>) -> String {
    let mut sections = BTreeMap::new();
    sections.insert(
        "General".to_owned(),
        BTreeMap::from([
            ("OutputAPI".to_owned(), "bestavailable".to_owned()),
            ("FullScreenMode".to_owned(), "true".to_owned()),
            ("ScalingMode".to_owned(), "stretched_ar".to_owned()),
        ]),
    );
    sections.insert(
        "DirectX".to_owned(),
        BTreeMap::from([
            ("DisableAndPassThru".to_owned(), "false".to_owned()),
            ("dgVoodooWatermark".to_owned(), "false".to_owned()),
            ("FastVideoMemoryAccess".to_owned(), "true".to_owned()),
        ]),
    );
    sections.insert(
        "Glide".to_owned(),
        BTreeMap::from([
            ("VideoCard".to_owned(), "voodoo_2".to_owned()),
            ("OnboardRAM".to_owned(), "12".to_owned()),
        ]),
    );

    merge_sections(&mut sections, overrides);
    serialize_crlf_ini(&sections)
}

/// Generates `dxwrapper.ini` with compatibility stub enables and recipe `overrides`.
pub fn render_dxwrapper_ini(
    stub_dlls: &[String],
    overrides: &BTreeMap<String, BTreeMap<String, String>>,
) -> String {
    let mut general = BTreeMap::from([("RealDllPath".to_owned(), "Auto".to_owned())]);
    let mut compat = BTreeMap::new();

    for stub in stub_dlls {
        let lower = stub.to_ascii_lowercase();
        match lower.as_str() {
            "ddraw.dll" => {
                compat.insert("Dd7to9".to_owned(), "1".to_owned());
            }
            "d3d8.dll" => {
                compat.insert("D3d8to9".to_owned(), "1".to_owned());
            }
            "dinput8.dll" | "dinput.dll" => {
                compat.insert("Dinputto8".to_owned(), "1".to_owned());
            }
            "dsound.dll" => {
                compat.insert("DsoundCtrl".to_owned(), "1".to_owned());
            }
            _ => {
                general.insert(format!("LoadCustomDll_{stub}"), "1".to_owned());
            }
        }
    }

    let mut sections = BTreeMap::new();
    sections.insert("General".to_owned(), general);
    if !compat.is_empty() {
        sections.insert("Compatibility".to_owned(), compat);
    }

    merge_sections(&mut sections, overrides);
    serialize_crlf_ini(&sections)
}

/// Generates `XInputPlus.ini` from recipe controller profile sections.
pub fn render_xinput_plus_ini(profile_ini: &BTreeMap<String, BTreeMap<String, String>>) -> String {
    let mut sections = BTreeMap::from([(
        "XInput".to_owned(),
        BTreeMap::from([
            ("UseLeftStick".to_owned(), "true".to_owned()),
            ("Vibration".to_owned(), "true".to_owned()),
        ]),
    )]);
    merge_sections(&mut sections, profile_ini);
    serialize_crlf_ini(&sections)
}

/// Generates a per-game `dosbox-x.conf` configuration profile.
pub fn render_dosbox_x_conf(
    machine: &str,
    os_profile: LegacyOsProfile,
    overrides: &BTreeMap<String, BTreeMap<String, String>>,
) -> String {
    let ver = match os_profile {
        LegacyOsProfile::Dos => "6.22",
        LegacyOsProfile::Win31 => "3.11",
        LegacyOsProfile::Win95 => "95",
        LegacyOsProfile::Win98 => "98",
    };

    let mut sections = BTreeMap::from([
        (
            "dosbox".to_owned(),
            BTreeMap::from([
                ("machine".to_owned(), machine.to_owned()),
                ("memsize".to_owned(), "64".to_owned()),
            ]),
        ),
        ("dos".to_owned(), BTreeMap::from([("ver".to_owned(), ver.to_owned())])),
        (
            "cpu".to_owned(),
            BTreeMap::from([
                ("core".to_owned(), "dynamic".to_owned()),
                ("cycles".to_owned(), "auto".to_owned()),
            ]),
        ),
    ]);

    merge_sections(&mut sections, overrides);
    serialize_crlf_ini(&sections)
}

/// Generates a per-game `86box.cfg` machine configuration file.
pub fn render_86box_cfg(
    machine_profile: &str,
    os_profile: LegacyOsProfile,
    overrides: &BTreeMap<String, BTreeMap<String, String>>,
) -> String {
    let os_hint = match os_profile {
        LegacyOsProfile::Dos => "dos",
        LegacyOsProfile::Win31 => "win31",
        LegacyOsProfile::Win95 => "win95",
        LegacyOsProfile::Win98 => "win98",
    };

    let mut sections = BTreeMap::from([(
        "Machine".to_owned(),
        BTreeMap::from([
            ("machine".to_owned(), machine_profile.to_owned()),
            ("ogsc_os_profile".to_owned(), os_hint.to_owned()),
        ]),
    )]);

    merge_sections(&mut sections, overrides);
    serialize_crlf_ini(&sections)
}

fn merge_sections(
    base: &mut BTreeMap<String, BTreeMap<String, String>>,
    overrides: &BTreeMap<String, BTreeMap<String, String>>,
) {
    for (sec_name, kv_map) in overrides {
        let existing_sec = base.keys().find(|k| k.eq_ignore_ascii_case(sec_name)).cloned();
        let target_sec = match existing_sec {
            Some(key) => base.get_mut(&key).expect("key exists"),
            None => base.entry(sec_name.clone()).or_default(),
        };

        for (key, value) in kv_map {
            if let Some(existing_key) =
                target_sec.keys().find(|k| k.eq_ignore_ascii_case(key)).cloned()
            {
                target_sec.remove(&existing_key);
            }
            target_sec.insert(key.clone(), value.clone());
        }
    }
}

fn serialize_crlf_ini(sections: &BTreeMap<String, BTreeMap<String, String>>) -> String {
    let mut out = String::new();
    for (idx, (section, entries)) in sections.iter().enumerate() {
        if idx > 0 {
            out.push_str("\r\n");
        }
        out.push('[');
        out.push_str(section);
        out.push_str("]\r\n");
        for (key, val) in entries {
            out.push_str(key);
            out.push_str(" = ");
            out.push_str(val);
            out.push_str("\r\n");
        }
    }
    out
}
