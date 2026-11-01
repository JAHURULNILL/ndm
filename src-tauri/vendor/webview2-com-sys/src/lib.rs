#[allow(
    non_snake_case,
    non_upper_case_globals,
    non_camel_case_types,
    dead_code,
    clippy::all
)]
pub mod Microsoft {
    pub mod Web {
        pub mod WebView2 {
            pub mod Win32 {
                mod windows_core {
                    macro_rules! link_webview2 {
                        ($library:literal $abi:literal fn $($function:tt)*) => (
                            // NDM PATCH (vendored webview2-com-sys 0.39.1):
                            // upstream links WebView2LoaderStatic.lib on
                            // MSVC. That prebuilt archive (Chromium-built)
                            // drags dynamic-UCRT references into the final
                            // exe — ndm-app.exe imported seven
                            // api-ms-win-crt-*.dll forwarders even with
                            // +crt-static, so it could not start on
                            // Windows 7/8 (no in-box UCRT). The DLL
                            // loader's import lib resolves the same five
                            // entry points against WebView2Loader.dll,
                            // which itself imports only KERNEL32.dll.
                            // WebView2Loader.dll is shipped beside
                            // ndm-app.exe (portable ZIP + installer).
                            #[cfg_attr(
                                target_env = "msvc",
                                link(name = "WebView2Loader.dll")
                            )]
                            #[cfg_attr(
                                not(target_env = "msvc"),
                                link(name = "WebView2Loader.dll")
                            )]
                            extern $abi {
                                pub fn $($function)*;
                            }
                        )
                    }

                    pub(crate) use ::windows_core::*;
                    pub(crate) use link_webview2 as link;
                }

                include!("bindings.rs");
            }
        }
    }
}

pub mod declared_interfaces;

#[cfg(test)]
mod test {
    use windows_core::w;

    use crate::Microsoft::Web::WebView2::Win32::*;

    #[test]
    fn compare_eq() {
        let mut result = 1;
        unsafe { CompareBrowserVersions(w!("1.0.0"), w!("1.0.0"), &mut result) }.unwrap();
        assert_eq!(0, result);
    }

    #[test]
    fn compare_lt() {
        let mut result = 0;
        unsafe { CompareBrowserVersions(w!("1.0.0"), w!("1.0.1"), &mut result) }.unwrap();
        assert_eq!(-1, result);
    }

    #[test]
    fn compare_gt() {
        let mut result = 0;
        unsafe { CompareBrowserVersions(w!("2.0.0"), w!("1.0.1"), &mut result) }.unwrap();
        assert_eq!(1, result);
    }
}
