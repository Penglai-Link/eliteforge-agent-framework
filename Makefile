# 统一入口；GNU Make 3.81。组件依赖顺序由 Cargo workspace 管理。
.DEFAULT_GOAL := help

VERSION ?= $(_WORKSPACE_VERSION)
SKIP_TESTS ?= false
TEST ?=
ALL_FEATURES ?= false

ifeq ($(OS),Windows_NT)
_NULL := NUL
_FAIL := exit /b 1
else
_NULL := /dev/null
_FAIL := exit 1
endif

# pkgid 只读取本地清单和锁文件；不改版本，不解析远程依赖。
# 不重定向到 NUL，便于在 POSIX 上静态检查 OS=Windows_NT 分支。
_HASH := \#
_PACKAGE_ID := $(if $(and $(wildcard Cargo.toml),$(wildcard Cargo.lock)),$(shell cargo pkgid --locked --offline --quiet -p eliteforge-agent-framework))
_WORKSPACE_VERSION := $(if $(findstring $(_HASH),$(_PACKAGE_ID)),$(lastword $(subst $(_HASH), ,$(_PACKAGE_ID))))
_CARGO_FLAGS := --locked --target-dir target
_WORKSPACE_FLAGS := $(strip --workspace $(if $(filter true,$(ALL_FEATURES)),--all-features))
_RUSTDOCFLAGS := $(RUSTDOCFLAGS)
_TEST_TARGETS := $(sort $(basename $(notdir $(wildcard crates/*/tests/*.rs))))

ifneq ($(SKIP_TESTS),true)
ifneq ($(SKIP_TESTS),false)
$(error 错误：SKIP_TESTS 只接受 true 或 false)
endif
endif

ifneq ($(ALL_FEATURES),true)
ifneq ($(ALL_FEATURES),false)
$(error 错误：ALL_FEATURES 只接受 true 或 false)
endif
endif

# 按集成测试文件名选择套件，兼容仓库内两个 harness=false 测试程序。
ifneq ($(TEST),)
ifeq ($(filter test,$(MAKECMDGOALS)),)
$(error 错误：TEST 仅适用于 make test)
endif
ifneq ($(words $(TEST)),1)
$(error 错误：TEST 必须是一个集成测试文件名，不含 .rs)
endif
ifneq ($(filter-out $(_TEST_TARGETS),$(TEST)),)
$(error 错误：TEST 必须对应 crates/*/tests 下已有的 .rs 文件，不含扩展名)
endif
endif

.PHONY: help doctor install clean format lint test build dev deploy _version

help:
	@echo 统一命令：
	@echo   make help       显示帮助
	@echo   make doctor     只读检查 Rust、C 工具链、锁文件和发布条件
	@echo   make install    使用 Cargo.lock 拉取依赖
	@echo   make clean      清理本仓库 target 目录，保留锁文件和运行数据
	@echo   make format     格式化全部 Rust 源码
	@echo   make lint       格式检查、Clippy、编译检查和严格 Rustdoc 检查
	@echo   make test       workspace 测试、文档测试及 Framework 无默认特性测试
	@echo   make build      构建全部 workspace 库的 release 产物
	@echo   make dev        运行离线 standalone 示例：19 加 23，数据保存在 agent.sqlite
	@echo   make deploy     发布版本化制品；当前缺少发布配置，执行会报错
	@echo 能力：未检测到 Docker、Compose 或应用壳，不生成对应目标
	@echo 覆盖变量：
	@echo   VERSION        默认读取 Cargo.toml 的 workspace.package.version；适用于 build、deploy
	@echo                  只接受与清单一致的版本；Cargo 不支持命令行修改包版本
	@echo   SKIP_TESTS     默认 false，仅允许 true 或 false；适用于 install、build、deploy
	@echo                  cargo fetch 和 cargo build 不隐含测试；当前 deploy 不可用
	@echo                  显式 make test 始终执行测试，不受 SKIP_TESTS 影响
	@echo   TEST           默认空，执行全部测试；仅适用于 test
	@echo                  非空时选择一个集成测试套件，例如 TEST=container，不含 .rs
	@echo   ALL_FEATURES   默认 false，仅允许 true 或 false；适用于 lint、test、build
	@echo                  true 启用全部特性，包括可选 SQLite 原生资源及其完整性检查
	@echo 工具链：仓库已验证 Rust/Cargo 1.95.0，尚未声明最低支持版本

doctor:
	@echo 检查：统一本地路径需要 Cargo、rustc、rustfmt、Clippy 和 C 工具链
ifeq ($(OS),Windows_NT)
	@where cargo >$(_NULL) 2>&1 || (echo 错误：缺少 cargo，请准备 Rust 工具链 & $(_FAIL))
	@where rustc >$(_NULL) 2>&1 || (echo 错误：缺少 rustc，请准备 Rust 工具链 & $(_FAIL))
	@rustc -vV | findstr /c:"-pc-windows-msvc" >$(_NULL) && (where cl >$(_NULL) 2>&1 && where lib >$(_NULL) 2>&1 && where link >$(_NULL) 2>&1) || (rustc -vV | findstr /c:"-pc-windows-gnu" >$(_NULL) && where gcc >$(_NULL) 2>&1 && where ar >$(_NULL) 2>&1) || (echo 错误：MSVC 需要 cl、lib、link；GNU 需要 gcc、ar，请准备匹配的 Windows C 工具链 & $(_FAIL))
	@if not exist Cargo.lock (echo 错误：缺少 Cargo.lock，不能执行锁定构建 & $(_FAIL))
else
	@command -v cargo >$(_NULL) 2>&1 || { echo 错误：缺少 cargo，请准备 Rust 工具链; $(_FAIL); }
	@command -v rustc >$(_NULL) 2>&1 || { echo 错误：缺少 rustc，请准备 Rust 工具链; $(_FAIL); }
	@command -v cc >$(_NULL) 2>&1 || { echo 错误：SQLite bundled 构建需要 C 编译器 cc; $(_FAIL); }
	@command -v ar >$(_NULL) 2>&1 || { echo 错误：SQLite bundled 构建需要归档工具 ar; $(_FAIL); }
	@test -f Cargo.lock || { echo 错误：缺少 Cargo.lock，不能执行锁定构建; $(_FAIL); }
endif
	@cargo --version
	@rustc --version
ifeq ($(OS),Windows_NT)
	@rustdoc --version || (echo 错误：缺少 rustdoc，请准备 Rust 文档工具 & $(_FAIL))
	@cargo fmt --version || (echo 错误：缺少 rustfmt，请准备 Rust 格式化组件 & $(_FAIL))
	@cargo clippy --version || (echo 错误：缺少 Clippy，请准备 Rust 静态检查组件 & $(_FAIL))
	@cargo metadata --locked --offline --no-deps --format-version 1 >$(_NULL) || (echo 错误：Cargo workspace 清单无法读取 & $(_FAIL))
else
	@rustdoc --version || { echo 错误：缺少 rustdoc，请准备 Rust 文档工具; $(_FAIL); }
	@cargo fmt --version || { echo 错误：缺少 rustfmt，请准备 Rust 格式化组件; $(_FAIL); }
	@cargo clippy --version || { echo 错误：缺少 Clippy，请准备 Rust 静态检查组件; $(_FAIL); }
	@cargo metadata --locked --offline --no-deps --format-version 1 >$(_NULL) || { echo 错误：Cargo workspace 清单无法读取; $(_FAIL); }
endif
ifeq ($(_WORKSPACE_VERSION),)
	@echo 错误：无法从 Cargo 清单解析 workspace 版本
	@$(_FAIL)
else
	@echo 检查：workspace 版本为 $(_WORKSPACE_VERSION)，使用 Cargo.lock，无仓库 wrapper 或包管理器冲突
endif
	@echo 警告：尚未配置制品发布目标与流程，部分 crate 的 path 依赖缺少版本，deploy 不可用
	@echo 检查：未配置 Docker、Compose 或应用壳，本地目标不要求这些可选工具

install:
	@echo 检查：cargo fetch 不隐含测试，SKIP_TESTS 对此目标无测试可跳过
	cargo fetch --locked

clean:
	cargo clean --locked --offline --target-dir target

format:
	cargo fmt --all

lint: export RUSTDOCFLAGS := $(_RUSTDOCFLAGS) -D warnings
lint:
	cargo fmt --all -- --check
	cargo clippy $(_CARGO_FLAGS) $(_WORKSPACE_FLAGS) --all-targets -- -D warnings
	cargo check $(_CARGO_FLAGS) $(_WORKSPACE_FLAGS) --all-targets
	cargo doc $(_CARGO_FLAGS) $(_WORKSPACE_FLAGS) --no-deps

test:
ifeq ($(TEST),)
	cargo test $(_CARGO_FLAGS) $(_WORKSPACE_FLAGS) --all-targets
	cargo test $(_CARGO_FLAGS) $(_WORKSPACE_FLAGS) --doc
	cargo test $(_CARGO_FLAGS) -p eliteforge-agent-framework --no-default-features --all-targets
else
	cargo test $(_CARGO_FLAGS) $(_WORKSPACE_FLAGS) --test "$(TEST)"
endif

build: _version
	@echo 检查：cargo build 不隐含测试，SKIP_TESTS 对此目标无测试可跳过；测试使用 make test
	cargo build $(_CARGO_FLAGS) $(_WORKSPACE_FLAGS) --release

dev:
	cargo run $(_CARGO_FLAGS) -p eliteforge-agent-framework --example standalone -- 19 23

# 缺少发布目标、依赖版本和发布顺序证据，不能猜测为 crates.io 发布。
deploy: _version
	@echo 错误：尚未配置制品发布目标与流程，且部分 path 依赖缺少版本，无法发布 VERSION 对应制品
	@$(_FAIL)

_version:
ifeq ($(_WORKSPACE_VERSION),)
	@echo 错误：无法解析 Cargo 清单版本，请先运行 make doctor
	@$(_FAIL)
else ifneq ($(VERSION),$(_WORKSPACE_VERSION))
	@echo 错误：VERSION 必须与 Cargo.toml 的 workspace.package.version 一致；不支持动态改写包版本
	@$(_FAIL)
else
	@echo 检查：目标版本与 Cargo 清单一致，为 $(_WORKSPACE_VERSION)
endif
