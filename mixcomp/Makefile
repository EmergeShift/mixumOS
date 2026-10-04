# mixcomp 构建脚本。目标环境：Arch Linux（wlroots0.18 + libjpeg-turbo + json-c + libxkbcommon）。
# 不同发行版的 pc 名不同：Alpine=wlroots-0.18，Arch=wlroots0.18 或 wlroots。
WLROOTS_PC := $(shell for n in wlroots-0.18 wlroots0.18 wlroots; do pkg-config --exists $$n 2>/dev/null && echo $$n && break; done)
JSONC_CFLAGS := $(shell pkg-config --cflags json-c 2>/dev/null)
JSONC_LIBS   := $(shell pkg-config --libs json-c 2>/dev/null || echo -ljson-c)
JPEG_LIBS    := $(shell pkg-config --silence-errors --libs libjpeg 2>/dev/null || echo -ljpeg)
XKB_LIBS     := $(shell pkg-config --libs xkbcommon 2>/dev/null || echo -lxkbcommon)

CFLAGS  := -std=c11 -g -O2 -Wall -Wextra -Wno-unused-parameter \
	$(shell pkg-config --cflags $(WLROOTS_PC) wayland-server xkbcommon 2>/dev/null) \
	$(JSONC_CFLAGS) -D_GNU_SOURCE -DWLR_USE_UNSTABLE
LDLIBS  := $(shell pkg-config --libs $(WLROOTS_PC) wayland-server xkbcommon 2>/dev/null) \
	$(JSONC_LIBS) $(JPEG_LIBS) $(XKB_LIBS) -lwayland-client -lrt

SRCS := src/main.c src/window.c src/stream.c src/input.c
OBJS := $(SRCS:.c=.o)

mixcomp: $(OBJS)
	$(CC) -o $@ $(OBJS) $(LDLIBS)

%.o: %.c src/mixcomp.h
	$(CC) $(CFLAGS) -c -o $@ $<

run: mixcomp
	./mixcomp --sock /tmp/mixum-display.sock

clean:
	rm -f mixcomp $(OBJS)

.PHONY: run clean
