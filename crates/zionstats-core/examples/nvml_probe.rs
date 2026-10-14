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
                }
                Err(e) => println!("dev0 FAIL: {e:?}"),
            }
        }
        Err(e) => println!("init FAIL: {e:?}"),
    }
}
