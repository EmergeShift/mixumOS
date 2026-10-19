# mixcomp 构建脚本。目标环境：Arch Linux（wlroots0.18 + libjpeg-turbo + json-c + libxkbcommon）。
# 不同发行版的 pc 名不同：Alpine=wlroots-0.18，Arch=wlroots0.18 或 wlroots。
WLROOTS_PC := $(shell for n in wlroots-0.18 wlroots0.18 wlroots; do pkg-config --exists $$n 2>/dev/null && echo $$n && break; done)
JSONC_CFLAGS := $(shell pkg-config --cflags json-c 2>/dev/null)
JSONC_LIBS   := $(shell pkg-config --libs json-c 2>/dev/null || echo -ljson-c)
JPEG_LIBS    := $(shell pkg-config --silence-errors --libs libjpeg 2>/dev/null || echo -ljpeg)
PNG_CFLAGS   := $(shell pkg-config --cflags libpng 2>/dev/null)
PNG_LIBS     := $(shell pkg-config --libs libpng 2>/dev/null || echo -lpng)
XKB_LIBS     := $(shell pkg-config --libs xkbcommon 2>/dev/null || echo -lxkbcommon)
# H.264:pkg-config 探测到 openh264 才编入(否则纯 JPEG 路径,行为同 v1)
HAVE_OPENH264 := $(shell pkg-config --exists openh264 2>/dev/null && echo 1)
WAYLAND_SCANNER := wayland-scanner

CFLAGS  := -std=c11 -g -O2 -Wall -Wextra -Wno-unused-parameter -Isrc \
	$(shell pkg-config --cflags $(WLROOTS_PC) wayland-server xkbcommon 2>/dev/null) \
	$(JSONC_CFLAGS) $(PNG_CFLAGS) -D_GNU_SOURCE -DWLR_USE_UNSTABLE
LDLIBS  := $(shell pkg-config --libs $(WLROOTS_PC) wayland-server xkbcommon 2>/dev/null) \
	$(JSONC_LIBS) $(JPEG_LIBS) $(PNG_LIBS) $(XKB_LIBS) -lwayland-client -lrt

ifeq ($(HAVE_OPENH264),1)
CFLAGS += -DHAVE_OPENH264 $(shell pkg-config --cflags openh264 2>/dev/null)
LDLIBS += $(shell pkg-config --libs openh264 2>/dev/null)
endif

SRCS := src/main.c src/window.c src/stream.c src/input.c src/encoder_h264.c
OBJS := $(SRCS:.c=.o)
# 协议文件现场生成（发行版打包时 build() 会装 wayland-protocols，无需补丁）：
# - xdg-shell 头供 wlroots 头文件引用（wlr_xdg_shell.h）；
# - 注入客户端两个协议从头文件到私有码全套生成——上游 wayland-protocols
#   已移除 virtual-keyboard，本目录的 XML 与 wlroots 0.18.2 protocol/ 保持一致。
GEN_HEADERS := src/xdg-shell-protocol.h \
	src/virtual-keyboard-client-protocol.h src/wlr-virtual-pointer-client-protocol.h
GEN_PRIVOBJS := src/virtual-keyboard-protocol.o src/wlr-virtual-pointer-protocol.o

mixcomp: $(OBJS) $(GEN_PRIVOBJS)
	$(CC) -o $@ $(OBJS) $(GEN_PRIVOBJS) $(LDLIBS)

src/xdg-shell-protocol.h: /usr/share/wayland-protocols/stable/xdg-shell/xdg-shell.xml
	$(WAYLAND_SCANNER) server-header $< $@

# XML 文件名带 -unstable-v1 后缀而生成物名不带，无法用一条模式规则，逐个显式列出。
src/virtual-keyboard-client-protocol.h: protocol/virtual-keyboard-unstable-v1.xml
	$(WAYLAND_SCANNER) client-header $< $@

src/wlr-virtual-pointer-client-protocol.h: protocol/wlr-virtual-pointer-unstable-v1.xml
	$(WAYLAND_SCANNER) client-header $< $@

src/virtual-keyboard-protocol.c: protocol/virtual-keyboard-unstable-v1.xml
	$(WAYLAND_SCANNER) private-code $< $@

src/wlr-virtual-pointer-protocol.c: protocol/wlr-virtual-pointer-unstable-v1.xml
	$(WAYLAND_SCANNER) private-code $< $@

%.o: %.c src/mixcomp.h $(GEN_HEADERS)
	$(CC) $(CFLAGS) -c -o $@ $<

run: mixcomp
	./mixcomp --sock /tmp/mixum-display.sock

clean:
	rm -f mixcomp $(OBJS) $(GEN_PRIVOBJS) $(GEN_HEADERS) \
		src/virtual-keyboard-protocol.c src/wlr-virtual-pointer-protocol.c

.PHONY: run clean
