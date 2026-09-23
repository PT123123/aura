# Aura 开发常用命令

# 默认目标：构建
default: build

# 构建 debug 版本
build:
    cargo build

# 运行应用（debug）
run:
    cargo run

# 构建 release 版本
release:
    cargo build --release

# 单独拆成一步，是为了守住「目录名 == exe 内 CARGO_PKG_VERSION」这个不变量：
# 版本必须**先**写回 Cargo.toml，构建出来的 exe 才带新号；否则 C:\workshop\aura-<ver>
# 里的 exe 其实还是 <ver-1> 的号，目录名与二进制对不上。见 tools/deploy_workshop.py 顶部。
# 推进 Cargo.toml 的 patch 版本号（deploy-workshop 会在构建前调用）
bump-version:
    python tools/deploy_workshop.py --bump-only

# aura 自己只在识别出 Squirrel 安装（同级有 Update.exe）时才建快捷方式，装到 C:\workshop
# 落不到那条路上（见 src/installer/windows_squirrel.rs 的 ensure_startup_registered），
# 所以这里补上，并在每次 deploy-workshop 收尾时重指一次。
# 让桌面 / 开始菜单 / 开机自启三个快捷方式指向最新部署的构建
shortcuts:
    powershell -NoProfile -ExecutionPolicy Bypass -File tools/set_shortcuts.ps1

# 构建 release 并部署到 C:\workshop\aura-<版本>（每次 PATCH +1）
deploy-workshop: bump-version
    cargo build --release
    python tools/deploy_workshop.py --no-bump
    just shortcuts

# 清理构建产物
clean:
    cargo clean
