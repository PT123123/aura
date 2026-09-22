use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use naga::back::spv;
use naga::front::glsl;
use naga::valid::{Capabilities, ValidationFlags, Validator};
use naga::ShaderStage;

const TRAY_ICON_RESOURCE_ID: u16 = 101;
const NEXT_BACKGROUND_ICON_RESOURCE_ID: u16 = 203;
const REFRESH_ICON_RESOURCE_ID: u16 = 204;
const RELOAD_SETTINGS_ICON_RESOURCE_ID: u16 = 205;
const SETTINGS_ICON_RESOURCE_ID: u16 = 201;
const EXIT_ICON_RESOURCE_ID: u16 = 202;
const NEXT_BACKGROUND_ICON_FALLBACK_RESOURCE_ID: u16 = 303;
const REFRESH_ICON_FALLBACK_RESOURCE_ID: u16 = 304;
const RELOAD_SETTINGS_ICON_FALLBACK_RESOURCE_ID: u16 = 305;
const SETTINGS_ICON_FALLBACK_RESOURCE_ID: u16 = 301;
const EXIT_ICON_FALLBACK_RESOURCE_ID: u16 = 302;
const SHADER_VERTEX_SOURCE: &str = r#"#version 450
layout(location = 0) out vec2 aura_uv;

void main() {
    vec2 position;
    if (gl_VertexIndex == 0) {
        position = vec2(-1.0, -1.0);
    } else if (gl_VertexIndex == 1) {
        position = vec2(3.0, -1.0);
    } else {
        position = vec2(-1.0, 3.0);
    }
    aura_uv = position * 0.5 + 0.5;
    gl_Position = vec4(position, 0.0, 1.0);
}
"#;

const SHADER_UNIFORMS_SOURCE: &str = r#"
struct AuraUniforms {
    float time_seconds;
    float frame_index;
    float mouse_enabled;
    float padding;
    vec4 resolution;
    vec4 mouse;
};
"#;

const VULKAN_FRAGMENT_PREFIX: &str = r#"#version 450
layout(set = 0, binding = 0, std140) uniform AuraUniformBlock {
    float time_seconds;
    float frame_index;
    float mouse_enabled;
    float padding;
    vec4 resolution;
    vec4 mouse;
} aura_uniforms;
layout(location = 0) out vec4 aura_output;
"#;

const VULKAN_FRAGMENT_SUFFIX: &str = r#"
void main() {
    AuraUniforms uniforms = AuraUniforms(
        aura_uniforms.time_seconds,
        aura_uniforms.frame_index,
        aura_uniforms.mouse_enabled,
        aura_uniforms.padding,
        aura_uniforms.resolution,
        aura_uniforms.mouse
    );
    aura_output = aura_main(gl_FragCoord.xy + aura_uniforms.resolution.zw, uniforms);
}
"#;

fn main() {
    println!("cargo:rerun-if-changed=assets/tray.png");
    println!("cargo:rerun-if-changed=assets/menu-next-background.png");
    println!("cargo:rerun-if-changed=assets/menu-refresh.png");
    println!("cargo:rerun-if-changed=assets/rotate-reload.png");
    println!("cargo:rerun-if-changed=assets/menu-settings.png");
    println!("cargo:rerun-if-changed=assets/menu-exit.png");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-env-changed=AURA_VERSION_PRERELEASE");
    println!("cargo:rerun-if-env-changed=AURA_VERSION_METADATA");
    println!("cargo:rerun-if-env-changed=AURA_BUILD_DATE");
    println!("cargo:rerun-if-env-changed=QSB");

    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is required"));
    emit_version_metadata(&manifest_dir);

    let out_dir = PathBuf::from(
        std::env::var("OUT_DIR").expect("OUT_DIR is required for resource generation"),
    );

    let target = std::env::var("TARGET").unwrap_or_default();
    compile_precompiled_shaders(&manifest_dir, &out_dir, &target);
    if target.contains("windows") {
        let config = slint_build::CompilerConfiguration::new()
            .with_style("fluent-dark".into());
        slint_build::compile_with_config("ui/settings.slint", config)
            .expect("failed to compile settings UI");
        generate_windows_resources(&manifest_dir, &out_dir);
    }
}

fn compile_precompiled_shaders(manifest_dir: &Path, out_dir: &Path, target: &str) {
    let cores_dir = manifest_dir.join("shaders").join("cores");
    println!("cargo:rerun-if-changed={}", cores_dir.display());
    let shader_cores = discover_shader_cores(&cores_dir);
    if shader_cores.is_empty() {
        panic!("no .glsl shader cores found in {}", cores_dir.display());
    }

    let compiled_dir = out_dir.join("precompiled_shaders");
    fs::create_dir_all(&compiled_dir)
        .unwrap_or_else(|error| panic!("failed to create {}: {}", compiled_dir.display(), error));

    let vertex_spv = compiled_dir.join("aura_vertex.spv");
    compile_glsl_to_spirv(
        "shared vertex",
        SHADER_VERTEX_SOURCE,
        ShaderStage::Vertex,
        "main",
        &vertex_spv,
    );

    let linux_target = target.contains("linux");
    let qsb = linux_target.then(find_qsb).flatten();
    if linux_target && qsb.is_none() {
        panic!("Qt Shader Tools qsb is required for Plasma shader support; set QSB to its path");
    }

    let mut compiled = Vec::with_capacity(shader_cores.len());
    for (shader_name, core_path) in shader_cores {
        let core = fs::read_to_string(&core_path)
            .unwrap_or_else(|error| panic!("failed to read {}: {}", core_path.display(), error));
        let fragment_source = format!(
            "{VULKAN_FRAGMENT_PREFIX}\n{SHADER_UNIFORMS_SOURCE}\n{core}\n{VULKAN_FRAGMENT_SUFFIX}"
        );
        let fragment_spv = compiled_dir.join(format!("{shader_name}.fragment.spv"));
        compile_glsl_to_spirv(
            &shader_name,
            &fragment_source,
            ShaderStage::Fragment,
            "main",
            &fragment_spv,
        );

        let mut linux_assets = None;
        if let Some(qsb) = qsb.as_ref() {
            linux_assets = Some(generate_linux_shader_assets(
                &compiled_dir,
                &shader_name,
                &core,
                qsb,
            ));
        }
        compiled.push(CompiledShader {
            name: shader_name,
            vertex_spv: vertex_spv.clone(),
            fragment_spv,
            linux: linux_assets,
        });
    }

    write_shader_registry(out_dir, &compiled, linux_target);
}

#[derive(Debug)]
struct CompiledShader {
    name: String,
    vertex_spv: PathBuf,
    fragment_spv: PathBuf,
    linux: Option<LinuxShaderAssets>,
}

#[derive(Debug)]
struct LinuxShaderAssets {
    gnome_glsl: PathBuf,
    plasma_vertex_qsb: PathBuf,
    plasma_fragment_qsb: PathBuf,
}

fn discover_shader_cores(cores_dir: &Path) -> Vec<(String, PathBuf)> {
    let entries = fs::read_dir(cores_dir)
        .unwrap_or_else(|error| panic!("failed to read {}: {}", cores_dir.display(), error));
    let mut cores = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("glsl"))
        .filter_map(|path| {
            let name = path.file_stem()?.to_str()?.to_string();
            Some((name, path))
        })
        .collect::<Vec<_>>();
    cores.sort_by(|left, right| left.0.cmp(&right.0));
    cores
}

fn compile_glsl_to_spirv(
    label: &str,
    source: &str,
    stage: ShaderStage,
    entry_point: &str,
    output: &Path,
) {
    let mut frontend = glsl::Frontend::default();
    let module = frontend
        .parse(&glsl::Options::from(stage), source)
        .unwrap_or_else(|errors| panic!("failed to parse {label} GLSL: {errors}"));
    let info = Validator::new(ValidationFlags::all(), Capabilities::all())
        .validate(&module)
        .unwrap_or_else(|error| panic!("failed to validate {label} GLSL: {error}"));
    let options = spv::Options {
        lang_version: (1, 3),
        ..Default::default()
    };
    let words = spv::write_vec(
        &module,
        &info,
        &options,
        Some(&spv::PipelineOptions {
            shader_stage: stage,
            entry_point: entry_point.to_string(),
        }),
    )
    .unwrap_or_else(|error| panic!("failed to emit {label} SPIR-V: {error}"));
    let mut bytes = Vec::with_capacity(words.len() * size_of::<u32>());
    for word in words {
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    fs::write(output, bytes)
        .unwrap_or_else(|error| panic!("failed to write {}: {}", output.display(), error));
}

fn write_shader_registry(out_dir: &Path, compiled: &[CompiledShader], linux_target: bool) {
    let mut source = String::from("&[\n");
    for shader in compiled {
        let name = shader.name.replace('"', "\\\"");
        let vertex = rust_path_literal(&shader.vertex_spv);
        let fragment = rust_path_literal(&shader.fragment_spv);
        if linux_target {
            let linux = shader
                .linux
                .as_ref()
                .expect("Linux assets must be generated");
            let gnome = rust_path_literal(&linux.gnome_glsl);
            let plasma_vertex = rust_path_literal(&linux.plasma_vertex_qsb);
            let plasma_fragment = rust_path_literal(&linux.plasma_fragment_qsb);
            source.push_str(&format!(
                "    ShaderAssets::new(\"{name}\", include_bytes!(\"{vertex}\"), include_bytes!(\"{fragment}\"), include_str!(\"{gnome}\"), include_bytes!(\"{plasma_vertex}\"), include_bytes!(\"{plasma_fragment}\")),\n"
            ));
        } else {
            source.push_str(&format!(
                "    ShaderAssets::new(\"{name}\", include_bytes!(\"{vertex}\"), include_bytes!(\"{fragment}\")),\n"
            ));
        }
    }
    source.push_str("]\n");

    let registry_path = out_dir.join("precompiled_shaders.rs");
    fs::write(&registry_path, source).unwrap_or_else(|error| {
        panic!(
            "failed to write shader registry {}: {}",
            registry_path.display(),
            error
        )
    });
}

fn rust_path_literal(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .replace('"', "\\\"")
}

fn find_qsb() -> Option<PathBuf> {
    if let Some(configured) = std::env::var_os("QSB") {
        let path = PathBuf::from(configured);
        if command_is_available(&path) {
            return Some(path);
        }
        panic!(
            "QSB points to an unavailable executable: {}",
            path.display()
        );
    }

    [
        PathBuf::from("qsb"),
        PathBuf::from("qsb6"),
        PathBuf::from("/usr/lib/qt6/bin/qsb"),
        PathBuf::from("/usr/lib64/qt6/bin/qsb"),
        PathBuf::from("/usr/lib/x86_64-linux-gnu/qt6/bin/qsb"),
    ]
    .into_iter()
    .find(|candidate| command_is_available(candidate))
}

fn command_is_available(command: &Path) -> bool {
    Command::new(command)
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn generate_linux_shader_assets(
    compiled_dir: &Path,
    shader_name: &str,
    core: &str,
    qsb: &Path,
) -> LinuxShaderAssets {
    let gnome_glsl = compiled_dir.join(format!("{shader_name}.gnome.glsl"));
    let gnome_source = format!("{SHADER_UNIFORMS_SOURCE}\n{core}\n");
    fs::write(&gnome_glsl, gnome_source)
        .unwrap_or_else(|error| panic!("failed to write {}: {}", gnome_glsl.display(), error));

    let plasma_vertex = compiled_dir.join(format!("{shader_name}.plasma.vert"));
    let plasma_fragment = compiled_dir.join(format!("{shader_name}.plasma.frag"));
    let plasma_vertex_qsb = compiled_dir.join(format!("{shader_name}.plasma.vert.qsb"));
    let plasma_fragment_qsb = compiled_dir.join(format!("{shader_name}.plasma.frag.qsb"));

    let vertex_source = r#"#version 440
layout(location = 0) in vec4 qt_Vertex;
layout(location = 1) in vec2 qt_MultiTexCoord0;
layout(location = 0) out vec2 aura_uv;
layout(std140, binding = 0) uniform AuraQtUniforms {
    mat4 qt_Matrix;
    float qt_Opacity;
    float aura_time_seconds;
    float aura_frame_index;
    float aura_mouse_enabled;
    float aura_padding;
    vec4 aura_resolution;
    vec4 aura_mouse;
    vec4 aura_origin;
    float aura_color_space_srgb;
};
void main() {
    aura_uv = qt_MultiTexCoord0;
    gl_Position = qt_Matrix * qt_Vertex;
}
"#;
    let fragment_source = format!(
        r#"#version 440
layout(location = 0) in vec2 aura_uv;
layout(location = 0) out vec4 aura_output;
layout(std140, binding = 0) uniform AuraQtUniforms {{
    mat4 qt_Matrix;
    float qt_Opacity;
    float aura_time_seconds;
    float aura_frame_index;
    float aura_mouse_enabled;
    float aura_padding;
    vec4 aura_resolution;
    vec4 aura_mouse;
    vec4 aura_origin;
    float aura_color_space_srgb;
}};
{SHADER_UNIFORMS_SOURCE}
{core}
vec3 aura_linear_to_srgb(vec3 value) {{
    vec3 low = value * 12.92;
    vec3 high = 1.055 * pow(max(value, vec3(0.0)), vec3(1.0 / 2.4)) - vec3(0.055);
    return vec3(
        value.r <= 0.0031308 ? low.r : high.r,
        value.g <= 0.0031308 ? low.g : high.g,
        value.b <= 0.0031308 ? low.b : high.b
    );
}}
void main() {{
    AuraUniforms uniforms = AuraUniforms(
        aura_time_seconds,
        max(aura_frame_index, 0.0),
        max(aura_mouse_enabled, 0.0),
        max(aura_padding, 0.0),
        aura_resolution,
        aura_mouse
    );
    vec2 frag_coord = aura_origin.xy + floor(aura_uv * max(aura_origin.zw, vec2(1.0))) + vec2(0.5);
    vec4 result = aura_main(frag_coord, uniforms);
    if (aura_color_space_srgb > 0.5)
        result.rgb = aura_linear_to_srgb(result.rgb);
    aura_output = result * qt_Opacity;
}}
"#
    );
    fs::write(&plasma_vertex, vertex_source)
        .unwrap_or_else(|error| panic!("failed to write {}: {}", plasma_vertex.display(), error));
    fs::write(&plasma_fragment, fragment_source)
        .unwrap_or_else(|error| panic!("failed to write {}: {}", plasma_fragment.display(), error));

    run_qsb(qsb, &plasma_vertex, &plasma_vertex_qsb);
    run_qsb(qsb, &plasma_fragment, &plasma_fragment_qsb);

    LinuxShaderAssets {
        gnome_glsl,
        plasma_vertex_qsb,
        plasma_fragment_qsb,
    }
}

fn run_qsb(qsb: &Path, source: &Path, output: &Path) {
    let result = Command::new(qsb)
        .arg("-o")
        .arg(output)
        .arg(source)
        .output()
        .unwrap_or_else(|error| panic!("failed to run {}: {}", qsb.display(), error));
    if !result.status.success() {
        panic!(
            "qsb failed for {}\nstdout:\n{}\nstderr:\n{}",
            source.display(),
            String::from_utf8_lossy(&result.stdout).trim(),
            String::from_utf8_lossy(&result.stderr).trim()
        );
    }
    match fs::metadata(output) {
        Ok(metadata) if metadata.is_file() && metadata.len() > 0 => {}
        Ok(_) => panic!(
            "qsb produced an empty or invalid file at {}",
            output.display()
        ),
        Err(error) => panic!("qsb did not produce {}: {error}", output.display()),
    }
}

fn generate_windows_resources(manifest_dir: &Path, out_dir: &Path) {
    let source_png = manifest_dir.join("assets").join("tray.png");
    if !source_png.exists() {
        panic!("missing source tray image: {}", source_png.display());
    }
    let next_background_source_png = manifest_dir.join("assets").join("menu-next-background.png");
    if !next_background_source_png.exists() {
        panic!(
            "missing source menu next background image: {}",
            next_background_source_png.display()
        );
    }
    let refresh_source_png = manifest_dir.join("assets").join("menu-refresh.png");
    if !refresh_source_png.exists() {
        panic!(
            "missing source menu refresh image: {}",
            refresh_source_png.display()
        );
    }
    let reload_settings_source_png = manifest_dir.join("assets").join("rotate-reload.png");
    if !reload_settings_source_png.exists() {
        panic!(
            "missing source menu reload settings image: {}",
            reload_settings_source_png.display()
        );
    }
    let settings_source_png = manifest_dir.join("assets").join("menu-settings.png");
    if !settings_source_png.exists() {
        panic!(
            "missing source menu settings image: {}",
            settings_source_png.display()
        );
    }
    let exit_source_png = manifest_dir.join("assets").join("menu-exit.png");
    if !exit_source_png.exists() {
        panic!(
            "missing source menu exit image: {}",
            exit_source_png.display()
        );
    }

    let generated_ico = out_dir.join("tray.ico");
    generate_multi_size_ico(&source_png, &generated_ico);
    let generated_next_background_bmp = out_dir.join("menu-next-background.bmp");
    generate_menu_bitmap(&next_background_source_png, &generated_next_background_bmp);
    let generated_refresh_bmp = out_dir.join("menu-refresh.bmp");
    generate_menu_bitmap(&refresh_source_png, &generated_refresh_bmp);
    let generated_reload_settings_bmp = out_dir.join("menu-rotate-reload.bmp");
    generate_menu_bitmap(&reload_settings_source_png, &generated_reload_settings_bmp);
    let generated_settings_bmp = out_dir.join("menu-settings.bmp");
    generate_menu_bitmap(&settings_source_png, &generated_settings_bmp);
    let generated_exit_bmp = out_dir.join("menu-exit.bmp");
    generate_menu_bitmap(&exit_source_png, &generated_exit_bmp);
    let generated_next_background_ico = out_dir.join("menu-next-background.ico");
    generate_menu_icon(&next_background_source_png, &generated_next_background_ico);
    let generated_refresh_ico = out_dir.join("menu-refresh.ico");
    generate_menu_icon(&refresh_source_png, &generated_refresh_ico);
    let generated_reload_settings_ico = out_dir.join("menu-rotate-reload.ico");
    generate_menu_icon(&reload_settings_source_png, &generated_reload_settings_ico);
    let generated_settings_ico = out_dir.join("menu-settings.ico");
    generate_menu_icon(&settings_source_png, &generated_settings_ico);
    let generated_exit_ico = out_dir.join("menu-exit.ico");
    generate_menu_icon(&exit_source_png, &generated_exit_ico);

    let generated_rc = out_dir.join("aura-auto.rc");
    let ico_path_for_rc = generated_ico.to_string_lossy().replace('\\', "/");
    let next_background_bmp_path_for_rc = generated_next_background_bmp
        .to_string_lossy()
        .replace('\\', "/");
    let refresh_bmp_path_for_rc = generated_refresh_bmp.to_string_lossy().replace('\\', "/");
    let reload_settings_bmp_path_for_rc = generated_reload_settings_bmp
        .to_string_lossy()
        .replace('\\', "/");
    let settings_bmp_path_for_rc = generated_settings_bmp.to_string_lossy().replace('\\', "/");
    let exit_bmp_path_for_rc = generated_exit_bmp.to_string_lossy().replace('\\', "/");
    let next_background_ico_path_for_rc = generated_next_background_ico
        .to_string_lossy()
        .replace('\\', "/");
    let refresh_ico_path_for_rc = generated_refresh_ico.to_string_lossy().replace('\\', "/");
    let reload_settings_ico_path_for_rc = generated_reload_settings_ico
        .to_string_lossy()
        .replace('\\', "/");
    let settings_ico_path_for_rc = generated_settings_ico.to_string_lossy().replace('\\', "/");
    let exit_ico_path_for_rc = generated_exit_ico.to_string_lossy().replace('\\', "/");
    let rc_payload = format!(
        "{} ICON \"{}\"\n{} BITMAP \"{}\"\n{} BITMAP \"{}\"\n{} BITMAP \"{}\"\n{} BITMAP \"{}\"\n{} BITMAP \"{}\"\n{} ICON \"{}\"\n{} ICON \"{}\"\n{} ICON \"{}\"\n{} ICON \"{}\"\n{} ICON \"{}\"\n",
        TRAY_ICON_RESOURCE_ID,
        ico_path_for_rc,
        NEXT_BACKGROUND_ICON_RESOURCE_ID,
        next_background_bmp_path_for_rc,
        REFRESH_ICON_RESOURCE_ID,
        refresh_bmp_path_for_rc,
        RELOAD_SETTINGS_ICON_RESOURCE_ID,
        reload_settings_bmp_path_for_rc,
        SETTINGS_ICON_RESOURCE_ID,
        settings_bmp_path_for_rc,
        EXIT_ICON_RESOURCE_ID,
        exit_bmp_path_for_rc,
        NEXT_BACKGROUND_ICON_FALLBACK_RESOURCE_ID,
        next_background_ico_path_for_rc,
        REFRESH_ICON_FALLBACK_RESOURCE_ID,
        refresh_ico_path_for_rc,
        RELOAD_SETTINGS_ICON_FALLBACK_RESOURCE_ID,
        reload_settings_ico_path_for_rc,
        SETTINGS_ICON_FALLBACK_RESOURCE_ID,
        settings_ico_path_for_rc,
        EXIT_ICON_FALLBACK_RESOURCE_ID,
        exit_ico_path_for_rc
    );
    fs::write(&generated_rc, rc_payload).expect("failed to write generated rc file");

    let generated_rc_str = generated_rc
        .to_str()
        .expect("generated rc path must be valid UTF-8");
    let _ = embed_resource::compile(generated_rc_str, embed_resource::NONE);
}

fn generate_multi_size_ico(source_png: &Path, output_ico: &Path) {
    let source = image::open(source_png)
        .unwrap_or_else(|e| panic!("failed to load {}: {}", source_png.display(), e))
        .to_rgba8();

    let sizes = [16u32, 20, 24, 32, 48, 64, 256];

    let mut icon_dir = ico::IconDir::new(ico::ResourceType::Icon);
    for size in sizes {
        let resized =
            image::imageops::resize(&source, size, size, image::imageops::FilterType::Lanczos3);
        let icon_image = ico::IconImage::from_rgba_data(size, size, resized.into_raw());
        let entry = ico::IconDirEntry::encode(&icon_image)
            .unwrap_or_else(|e| panic!("failed to encode {}x{} icon entry: {}", size, size, e));
        icon_dir.add_entry(entry);
    }

    let mut file = fs::File::create(output_ico)
        .unwrap_or_else(|e| panic!("failed to create {}: {}", output_ico.display(), e));
    icon_dir
        .write(&mut file)
        .unwrap_or_else(|e| panic!("failed to write {}: {}", output_ico.display(), e));
}

fn generate_menu_bitmap(source_png: &Path, output_bmp: &Path) {
    let source = image::open(source_png)
        .unwrap_or_else(|e| panic!("failed to load {}: {}", source_png.display(), e))
        .to_rgba8();
    let resized = image::imageops::resize(&source, 16, 16, image::imageops::FilterType::Lanczos3);
    image::DynamicImage::ImageRgba8(resized)
        .save_with_format(output_bmp, image::ImageFormat::Bmp)
        .unwrap_or_else(|e| panic!("failed to write {}: {}", output_bmp.display(), e));
}

fn generate_menu_icon(source_png: &Path, output_ico: &Path) {
    let source = image::open(source_png)
        .unwrap_or_else(|e| panic!("failed to load {}: {}", source_png.display(), e))
        .to_rgba8();
    let resized = image::imageops::resize(&source, 16, 16, image::imageops::FilterType::Lanczos3);

    let mut icon_dir = ico::IconDir::new(ico::ResourceType::Icon);
    let icon_image = ico::IconImage::from_rgba_data(16, 16, resized.into_raw());
    let entry = ico::IconDirEntry::encode(&icon_image)
        .unwrap_or_else(|e| panic!("failed to encode 16x16 icon entry: {}", e));
    icon_dir.add_entry(entry);

    let mut file = fs::File::create(output_ico)
        .unwrap_or_else(|e| panic!("failed to create {}: {}", output_ico.display(), e));
    icon_dir
        .write(&mut file)
        .unwrap_or_else(|e| panic!("failed to write {}: {}", output_ico.display(), e));
}

fn emit_version_metadata(manifest_dir: &Path) {
    let git_commit = run_git(&["rev-parse", "--short", "HEAD"]).unwrap_or_default();
    let git_branch = run_git(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();

    let build_date = std::env::var("AURA_BUILD_DATE")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(current_unix_timestamp);

    let version_prerelease = std::env::var("AURA_VERSION_PRERELEASE").unwrap_or_default();
    let version_metadata = std::env::var("AURA_VERSION_METADATA").unwrap_or_default();
    let publisher = read_publisher_from_nuspec(manifest_dir);

    println!("cargo:rustc-env=AURA_GIT_COMMIT={git_commit}");
    println!("cargo:rustc-env=AURA_GIT_BRANCH={git_branch}");
    println!("cargo:rustc-env=AURA_BUILD_DATE={build_date}");
    println!("cargo:rustc-env=AURA_VERSION_PRERELEASE={version_prerelease}");
    println!("cargo:rustc-env=AURA_VERSION_METADATA={version_metadata}");
    println!("cargo:rustc-env=AURA_PUBLISHER={publisher}");
}

fn read_publisher_from_nuspec(manifest_dir: &Path) -> String {
    let nuspec_path = manifest_dir
        .join("packaging")
        .join("windows")
        .join("squirrel")
        .join("aura.nuspec");
    println!("cargo:rerun-if-changed={}", nuspec_path.display());

    let nuspec_contents = fs::read_to_string(&nuspec_path).unwrap_or_else(|error| {
        panic!(
            "failed to read nuspec metadata {}: {}",
            nuspec_path.display(),
            error
        )
    });

    extract_xml_element(&nuspec_contents, "authors")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            panic!(
                "missing non-empty <authors> value in {}",
                nuspec_path.display()
            )
        })
        .to_string()
}

fn extract_xml_element<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let start_tag = format!("<{tag}>");
    let end_tag = format!("</{tag}>");

    let start_index = text.find(&start_tag)? + start_tag.len();
    let end_index = text[start_index..].find(&end_tag)? + start_index;
    Some(&text[start_index..end_index])
}

fn run_git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn current_unix_timestamp() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs().to_string(),
        Err(_) => String::new(),
    }
}
