fn main() {
    wifi_credentials();
    linker_be_nice();
    // 不链接 defmt.x：这个固件不用 defmt，日志走串口（见 Cargo.toml 里的说明）
    // make sure linkall.x is the last linker script (otherwise might cause problems with flip-link)
    println!("cargo:rustc-link-arg=-Tlinkall.x");
}

/// 把 WiFi 账号密码从 `wifi.toml` 读出来编译进固件。
///
/// 为什么不直接写在源码里：这个仓库有 GitHub Actions，是要推上去的，
/// 家里 WiFi 的密码不能跟着一起进版本库。`wifi.toml` 在 `.gitignore` 里，
/// 照着 `wifi.toml.example` 复制一份填上就行。
///
/// CI 上没有这个文件，那就留空 —— 固件照样能编译，只是开机会打一条日志
/// 提醒你没配 WiFi，而不是编译失败。
fn wifi_credentials() {
    println!("cargo:rerun-if-changed=wifi.toml");
    println!("cargo:rerun-if-env-changed=WIFI_SSID");
    println!("cargo:rerun-if-env-changed=WIFI_PASSWORD");

    println!("cargo:rerun-if-env-changed=TV_URL");
    println!("cargo:rerun-if-env-changed=TV_IP");
    println!("cargo:rerun-if-env-changed=WIFI_BSSID");
    println!("cargo:rerun-if-env-changed=WIFI_CHANNEL");

    let text = std::fs::read_to_string("wifi.toml").unwrap_or_default();
    let mut ssid = String::new();
    let mut password = String::new();
    let mut tv_url = String::new();
    let mut tv_ip = String::new();
    let mut bssid = String::new();
    let mut channel = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_string();
        match key.trim() {
            "ssid" => ssid = value,
            "password" => password = value,
            "tv_url" => tv_url = value,
            "tv_ip" => tv_ip = value,
            "bssid" => bssid = value,
            "channel" => channel = value,
            _ => {}
        }
    }

    // 环境变量优先，方便临时换一个网络 / 换一台设备试
    let ssid = std::env::var("WIFI_SSID").unwrap_or(ssid);
    let password = std::env::var("WIFI_PASSWORD").unwrap_or(password);
    let tv_url = std::env::var("TV_URL").unwrap_or(tv_url);
    let tv_ip = std::env::var("TV_IP").unwrap_or(tv_ip);
    let bssid = std::env::var("WIFI_BSSID").unwrap_or(bssid);
    let channel = std::env::var("WIFI_CHANNEL").unwrap_or(channel);

    // 只有**真的配错了**才用 cargo:warning。
    //
    // 构建脚本除了 cargo:warning 没有别的办法往终端打字，很容易顺手拿它
    // 打提示，结果就是每次编译都刷一屏「警告」，真正的警告反而被淹掉。
    // 「用哪台电视」「锁了哪个 BSSID」这些固件开机时会原样打到串口上，
    // 不需要在编译期再说一遍。
    if ssid.is_empty() {
        println!(
            "cargo:warning=没有找到 WiFi 配置：复制 firmware/wifi.toml.example 成 wifi.toml 并填上账号密码"
        );
    }
    println!("cargo:rustc-env=WIFI_SSID={ssid}");
    println!("cargo:rustc-env=WIFI_PASSWORD={password}");
    println!("cargo:rustc-env=TV_URL={tv_url}");
    println!("cargo:rustc-env=TV_IP={tv_ip}");
    println!("cargo:rustc-env=WIFI_BSSID={bssid}");
    println!("cargo:rustc-env=WIFI_CHANNEL={channel}");
}

fn linker_be_nice() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        let kind = &args[1];
        let what = &args[2];

        match kind.as_str() {
            "undefined-symbol" => match what.as_str() {
                what if what.starts_with("_defmt_") => {
                    eprintln!();
                    eprintln!(
                        "💡 `defmt` not found - make sure `defmt.x` is added as a linker script and you have included `use defmt_rtt as _;`"
                    );
                    eprintln!();
                }
                "_stack_start" => {
                    eprintln!();
                    eprintln!("💡 Is the linker script `linkall.x` missing?");
                    eprintln!();
                }
                what if what.starts_with("esp_rtos_") => {
                    eprintln!();
                    eprintln!(
                        "💡 `esp-radio` has no scheduler enabled. Make sure you have initialized `esp-rtos` or provided an external scheduler."
                    );
                    eprintln!();
                }
                "embedded_test_linker_file_not_added_to_rustflags" => {
                    eprintln!();
                    eprintln!(
                        "💡 `embedded-test` not found - make sure `embedded-test.x` is added as a linker script for tests"
                    );
                    eprintln!();
                }
                "free"
                | "malloc"
                | "calloc"
                | "get_free_internal_heap_size"
                | "malloc_internal"
                | "realloc_internal"
                | "calloc_internal"
                | "free_internal" => {
                    eprintln!();
                    eprintln!(
                        "💡 Did you forget the `esp-alloc` dependency or didn't enable the `compat` feature on it?"
                    );
                    eprintln!();
                }
                _ => (),
            },
            // we don't have anything helpful for "missing-lib" yet
            _ => {
                std::process::exit(1);
            }
        }

        std::process::exit(0);
    }

    println!(
        "cargo:rustc-link-arg=-Wl,--error-handling-script={}",
        std::env::current_exe().unwrap().display()
    );
}
