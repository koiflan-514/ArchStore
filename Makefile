# ArchStore — 构建、检查、翻译与安装（不使用 Meson：纯 Rust 项目用 Cargo 足矣）
#
# 常用目标：
#   make build        构建 release 二进制
#   make check        CI 检查清单（§11.3）
#   make test         全部测试
#   make install      DESTDIR 安装（供 PKGBUILD 使用）
#   make uninstall    卸载

PREFIX      ?= /usr
DESTDIR     ?=
CARGO       ?= cargo
INSTALL     ?= install
LIBEXECDIR  ?= $(PREFIX)/lib
DATADIR     ?= $(PREFIX)/share
BINDIR      ?= $(PREFIX)/bin
LOCALEDIR   ?= $(DATADIR)/locale
APPDIR      ?= $(DATADIR)/applications
METAINFODIR ?= $(DATADIR)/metainfo
ICONDIR     ?= $(DATADIR)/icons/hicolor
POLICYDIR   ?= $(DATADIR)/polkit-1/actions
GSSCHEMADIR ?= $(DATADIR)/glib-2.0/schemas
ARCHSTOREDIR?= $(DATADIR)/archstore
HELPERDIR   := $(LIBEXECDIR)/archstore

APPS    := target/release/archstore
HELPER  := target/release/archstore-helper

.PHONY: all build check test fmt clippy pot mo install uninstall clean run doctor helper perf

all: build

build:
	$(CARGO) build --release --workspace

$(APPS) $(HELPER): build

# --- 翻译 ---
# POT 抽取固定命令（§7.2）
pot:
	xgettext --from-code=UTF-8 --keyword=t --keyword=n \
	  -o po/archstore.pot $$(find crates -name '*.rs')
	@for po in po/*.po; do msgmerge --update "$$po" po/archstore.pot; done

# 编译 .mo 到源码树（开发用；安装时由 install 目标写入 DESTDIR）
mo:
	@for po in po/*.po; do \
	  lang=$$(basename $$po .po); \
	  mkdir -p po/$$lang/LC_MESSAGES; \
	  msgfmt --check -o po/$$lang/LC_MESSAGES/archstore.mo $$po; \
	done

# --- 质量保障（§11.3 CI 检查清单）---
fmt:
	$(CARGO) fmt --all

check:
	$(CARGO) fmt --all --check
	$(CARGO) clippy --workspace --all-targets -- -D warnings
	$(CARGO) test --workspace
	@if $(CARGO) tree -p archstore-core | grep -qE '(^|[^a-z-])gtk4 '; then \
	  echo "错误：archstore-core 不得依赖 gtk4（依赖方向约束 §3.2）"; exit 1; \
	fi
	@for po in po/*.po; do msgfmt --check -o /dev/null "$$po"; done
	@echo "所有检查通过"

test:
	$(CARGO) test --workspace

clippy:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

run:
	$(CARGO) run -p archstore-gui --bin archstore

doctor: build
	./$(APPS) --doctor

helper: build
	./$(HELPER) --version

# 性能回归（§12 阶段 5）：延迟预算 / 复杂度护栏 / 常驻内存
# 先跑 release 模式的测试（真实运行形态），再测内存。
perf: build
	$(CARGO) test --release -p archstore-core --test perf -- --nocapture --test-threads=1
	scripts/measure-memory.sh

# --- 安装 ---
install: build mo
	$(INSTALL) -Dm755 $(APPS)   $(DESTDIR)$(BINDIR)/archstore
	$(INSTALL) -Dm755 $(HELPER) $(DESTDIR)$(HELPERDIR)/archstore-helper
	$(INSTALL) -Dm644 data/io.github.archstore.ArchStore.desktop \
	  $(DESTDIR)$(APPDIR)/io.github.archstore.ArchStore.desktop
	$(INSTALL) -Dm644 data/io.github.archstore.ArchStore.metainfo.xml \
	  $(DESTDIR)$(METAINFODIR)/io.github.archstore.ArchStore.metainfo.xml
	$(INSTALL) -Dm644 data/io.github.archstore.ArchStore.gschema.xml \
	  $(DESTDIR)$(GSSCHEMADIR)/io.github.archstore.ArchStore.gschema.xml
	$(INSTALL) -Dm644 data/io.github.archstore.ArchStore.policy \
	  $(DESTDIR)$(POLICYDIR)/io.github.archstore.ArchStore.policy
	$(INSTALL) -Dm644 i18n/software-names.json \
	  $(DESTDIR)$(ARCHSTOREDIR)/software-names.json
	@for po in po/*.po; do \
	  lang=$$(basename $$po .po); \
	  $(INSTALL) -Dm644 po/$$lang/LC_MESSAGES/archstore.mo \
	    $(DESTDIR)$(LOCALEDIR)/$$lang/LC_MESSAGES/archstore.mo; \
	done
	@if [ -f data/icons/io.github.archstore.ArchStore.svg ]; then \
	  $(INSTALL) -Dm644 data/icons/io.github.archstore.ArchStore.svg \
	    $(DESTDIR)$(ICONDIR)/scalable/apps/io.github.archstore.ArchStore.svg; \
	fi
	@glcs="$$(command -v glib-compile-schemas || true)"; \
	if [ -n "$$glcs" ] && [ -d "$(DESTDIR)$(GSSCHEMADIR)" ]; then \
	  $$glcs "$(DESTDIR)$(GSSCHEMADIR)" || true; \
	fi
	@echo "安装完成。运行 archstore 启动；提权功能需要 polkit（pkexec）。"

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/archstore
	rm -f $(DESTDIR)$(HELPERDIR)/archstore-helper
	-rmdir $(DESTDIR)$(HELPERDIR) 2>/dev/null || true
	rm -f $(DESTDIR)$(APPDIR)/io.github.archstore.ArchStore.desktop
	rm -f $(DESTDIR)$(METAINFODIR)/io.github.archstore.ArchStore.metainfo.xml
	rm -f $(DESTDIR)$(GSSCHEMADIR)/io.github.archstore.ArchStore.gschema.xml
	rm -f $(DESTDIR)$(POLICYDIR)/io.github.archstore.ArchStore.policy
	rm -f $(DESTDIR)$(ARCHSTOREDIR)/software-names.json
	@for po in po/*.po; do \
	  lang=$$(basename $$po .po); \
	  rm -f $(DESTDIR)$(LOCALEDIR)/$$lang/LC_MESSAGES/archstore.mo; \
	done

clean:
	$(CARGO) clean
	rm -rf po/*/LC_MESSAGES
