use nvml_wrapper::Nvml;
fn main() {
    let nvml = Nvml::init();
    match nvml {
        Ok(nvml) => {
            println!("init ok, driver={:?}", nvml.sys_driver_version());
            let count = nvml.device_count();
            println!("device_count={count:?}");
            match nvml.device_by_index(0) {
                Ok(d) => {
                    println!("dev0 ok");
                    println!("name={:?}", d.name());
                    println!("uuid={:?}", d.uuid().map(|u| u[..12].to_string()));
                    println!("util={:?}", d.utilization_rates());
                    println!(
                        "temp={:?}",
                        d.temperature(nvml_wrapper::enum_wrappers::device::TemperatureSensor::Gpu)
                    );
                    println!("pwr={:?}", d.power_usage());
                    println!(
                        "clock={:?}",
                        d.clock_info(nvml_wrapper::enum_wrappers::device::Clock::SM)
                    );
                    // v0.7: per-process utilization via the raw FFI
                    use zionstats_core::source::nvml_pmon::{NvmlPmonFfi, PmonSource};
                    if let Some(mut pmon) = NvmlPmonFfi::try_new() {
                        let raw = unsafe { d.handle() };
                        match pmon.fetch(raw, 0) {
                            Some(samples) => {
                                println!("pmon samples: {}", samples.len());
                                for s in samples.iter().take(8) {
                                    println!(
                                        "  pid={} ts={} sm={} mem={} enc={} dec={}",
                                        s.pid, s.ts_ms, s.sm, s.mem, s.enc, s.dec
                                    );
                                }
                            }
                            None => println!("pmon fetch FAILED (driver without the symbol?)"),
                        }
                    } else {
                        println!("pmon: nvmlDeviceGetProcessUtilization not available");
                    }
                }
                Err(e) => println!("dev0 FAIL: {e:?}"),
            }
        }
        Err(e) => println!("init FAIL: {e:?}"),
    }
}
