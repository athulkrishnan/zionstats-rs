//! Real-device Tuya readback probe. Reads id/key/address from the shared
//! config (never hardcodes secrets); STATUS ONLY -- never actuates.
use zionstats_core::config::{self, PlugCfg};
use zionstats_core::control::actuators::Plug;
use zionstats_core::control::plugs::tuya::TuyaPlug;

fn main() {
    let cfg = config::load(&config::config_path(None));
    let Some(p) = cfg.plugs.iter().find(|p| p.backend == "tuya") else {
        eprintln!("no tuya plug in config");
        std::process::exit(2);
    };
    println!(
        "probe target: {} @ {} v{:.1} (status-only, never actuates)",
        p.name,
        p.address.as_deref().unwrap_or("?"),
        p.version
    );
    let _ = std::fs::File::create("/dev/null"); // touch fs before timing
    let started = std::time::Instant::now();
    match TuyaPlug::from_config(&PlugCfg {
        backend: p.backend.clone(),
        name: p.name.clone(),
        host: p.host.clone(),
        gen: p.gen,
        channel: p.channel,
        id: p.id.clone(),
        key: p.key.clone(),
        address: p.address.clone(),
        version: p.version,
        base_url: None,
        token: None,
        entity_id: None,
    }) {
        Ok(Some(mut plug)) => {
            let st = plug.status();
            let pw = plug.power();
            println!("status: {st:?} ({} ms)", started.elapsed().as_millis());
            println!("power: {pw:?}");
            match st {
                Some(b) => println!("HARDWARE-VERIFIED: v{:.1} status readback = {b}", p.version),
                None => println!(
                    "readback returned None (device unreachable or session-key v3.4 variant)"
                ),
            }
        }
        _ => eprintln!("tuya plug config incomplete (id/key/address required)"),
    }
}
