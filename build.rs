fn main() {
    #[cfg(windows)]
    {
        let mut resource = winresource::WindowsResource::new();
        let icon = create_icon();
        resource.set_icon(icon.to_string_lossy().as_ref());
        resource.set("ProductName", "文件交换助手");
        resource.set("FileDescription", "文件交换助手");
        resource.set("LegalCopyright", "MIT License");
        resource.set("OriginalFilename", "GiantFileExchange.exe");
        if let Err(error) = resource.compile() {
            println!("cargo:warning=无法写入 Windows 版本资源：{error}");
        }
    }
}

#[cfg(windows)]
fn create_icon() -> std::path::PathBuf {
    use std::io::Write;

    let path = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap())
        .join("GiantFileExchange.ico");
    let mut data = Vec::new();
    data.extend_from_slice(&[0, 0, 1, 0, 1, 0]);
    let image_size = 40_u32 + 32 * 32 * 4 + 32 * 4;
    data.extend_from_slice(&[32, 32, 0, 0]);
    data.extend_from_slice(&1_u16.to_le_bytes());
    data.extend_from_slice(&32_u16.to_le_bytes());
    data.extend_from_slice(&image_size.to_le_bytes());
    data.extend_from_slice(&22_u32.to_le_bytes());
    data.extend_from_slice(&40_u32.to_le_bytes());
    data.extend_from_slice(&32_i32.to_le_bytes());
    data.extend_from_slice(&64_i32.to_le_bytes());
    data.extend_from_slice(&1_u16.to_le_bytes());
    data.extend_from_slice(&32_u16.to_le_bytes());
    data.extend_from_slice(&0_u32.to_le_bytes());
    data.extend_from_slice(&(32_u32 * 32 * 4).to_le_bytes());
    data.extend_from_slice(&0_i32.to_le_bytes());
    data.extend_from_slice(&0_i32.to_le_bytes());
    data.extend_from_slice(&0_u32.to_le_bytes());
    data.extend_from_slice(&0_u32.to_le_bytes());
    for y in (0..32).rev() {
        for x in 0..32 {
            let inside = (3..29).contains(&x) && (3..29).contains(&y);
            let arrow = (14..18).contains(&x) || ((10..22).contains(&x) && (17..21).contains(&y));
            let pixel = if inside && arrow {
                [255, 255, 255, 255]
            } else if inside {
                [235, 99, 37, 255]
            } else {
                [0, 0, 0, 0]
            };
            data.extend_from_slice(&pixel);
        }
    }
    data.extend(std::iter::repeat_n(0_u8, 32 * 4));
    std::fs::File::create(&path)
        .unwrap()
        .write_all(&data)
        .unwrap();
    path
}
