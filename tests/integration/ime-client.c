// ime-client.c —— text-input-v3 最小客户端（e2e 用）:
// 开一个 300x200 的白窗口，获得键盘焦点后启用 text-input，
// 把收到的 commit_string 与 done 打印到 stdout。
// 构建（容器内）:
//   wayland-scanner client-header <xdg-shell.xml> xdg-shell-client-protocol.h
//   wayland-scanner private-code <xdg-shell.xml> xdg-shell-protocol.c
//   wayland-scanner client-header <text-input-unstable-v3.xml> text-input-v3-client-protocol.h
//   wayland-scanner private-code <text-input-unstable-v3.xml> text-input-v3-protocol.c
//   cc ime-client.c *-protocol.c -lwayland-client -o ime-client
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
#include <wayland-client.h>

#include "text-input-v3-client-protocol.h"
#include "xdg-shell-client-protocol.h"

static struct wl_display *display;
static struct wl_compositor *compositor;
static struct wl_shm *shm;
static struct wl_seat *seat;
static struct xdg_wm_base *wm_base;
static struct zwp_text_input_manager_v3 *ti_mgr;
static struct zwp_text_input_v3 *ti;
static struct wl_surface *surface;
static struct xdg_surface *xdg_surface;
static struct xdg_toplevel *toplevel;
static struct wl_buffer *buffer;

static void log_line(const char *tag, const char *text) {
	printf("%s:%s\n", tag, text != NULL ? text : "");
	fflush(stdout);
}

static void ti_enter(void *data, struct zwp_text_input_v3 *t,
		struct wl_surface *s) {
	(void)data;
	(void)t;
	(void)s;
	log_line("TI_ENTER", "");
}

static void ti_leave(void *data, struct zwp_text_input_v3 *t,
		struct wl_surface *s) {
	(void)data;
	(void)t;
	(void)s;
	log_line("TI_LEAVE", "");
}

static void ti_preedit(void *data, struct zwp_text_input_v3 *t,
		const char *text, int32_t begin, int32_t end) {
	(void)data;
	(void)t;
	(void)begin;
	(void)end;
	log_line("PREEDIT", text);
}

static void ti_commit_string(void *data, struct zwp_text_input_v3 *t,
		const char *text) {
	(void)data;
	(void)t;
	log_line("COMMIT", text);
}

static void ti_delete_surrounding(void *data, struct zwp_text_input_v3 *t,
		uint32_t before, uint32_t after) {
	(void)data;
	(void)t;
	(void)before;
	(void)after;
}

static void ti_done(void *data, struct zwp_text_input_v3 *t, uint32_t serial) {
	(void)data;
	(void)t;
	printf("DONE:%u\n", serial);
	fflush(stdout);
}

static const struct zwp_text_input_v3_listener ti_listener = {
	.enter = ti_enter,
	.leave = ti_leave,
	.preedit_string = ti_preedit,
	.commit_string = ti_commit_string,
	.delete_surrounding_text = ti_delete_surrounding,
	.done = ti_done,
};

static void kb_keymap(void *data, struct wl_keyboard *kb, uint32_t format,
		int32_t fd, uint32_t size) {
	(void)data;
	(void)kb;
	(void)format;
	(void)size;
	close(fd);
}

static void kb_enter(void *data, struct wl_keyboard *kb, uint32_t serial,
		struct wl_surface *s, struct wl_array *keys) {
	(void)data;
	(void)kb;
	(void)serial;
	(void)s;
	(void)keys;
	log_line("KB_ENTER", "");
	if (ti != NULL) {
		zwp_text_input_v3_enable(ti);
		zwp_text_input_v3_commit(ti);
	}
}

static void kb_leave(void *data, struct wl_keyboard *kb, uint32_t serial,
		struct wl_surface *s) {
	(void)data;
	(void)kb;
	(void)serial;
	(void)s;
	log_line("KB_LEAVE", "");
	if (ti != NULL) {
		zwp_text_input_v3_disable(ti);
		zwp_text_input_v3_commit(ti);
	}
}

static void kb_key(void *data, struct wl_keyboard *kb, uint32_t serial,
		uint32_t time, uint32_t key, uint32_t state) {
	(void)data;
	(void)kb;
	(void)serial;
	(void)time;
	(void)key;
	(void)state;
}

static void kb_modifiers(void *data, struct wl_keyboard *kb, uint32_t serial,
		uint32_t dep, uint32_t lat, uint32_t lock, uint32_t group) {
	(void)data;
	(void)kb;
	(void)serial;
	(void)dep;
	(void)lat;
	(void)lock;
	(void)group;
}

static void kb_repeat_info(void *data, struct wl_keyboard *kb, int32_t rate,
		int32_t delay) {
	(void)data;
	(void)kb;
	(void)rate;
	(void)delay;
}

static const struct wl_keyboard_listener kb_listener = {
	.keymap = kb_keymap,
	.enter = kb_enter,
	.leave = kb_leave,
	.key = kb_key,
	.modifiers = kb_modifiers,
	.repeat_info = kb_repeat_info,
};

static void seat_capabilities(void *data, struct wl_seat *s, uint32_t caps) {
	(void)data;
	static struct wl_keyboard *keyboard;
	if ((caps & WL_SEAT_CAPABILITY_KEYBOARD) && keyboard == NULL) {
		keyboard = wl_seat_get_keyboard(s);
		wl_keyboard_add_listener(keyboard, &kb_listener, NULL);
	}
}

static void seat_name(void *data, struct wl_seat *s, const char *name) {
	(void)data;
	(void)s;
	(void)name;
}

static const struct wl_seat_listener seat_listener = {
	.capabilities = seat_capabilities,
	.name = seat_name,
};

static void wm_ping(void *data, struct xdg_wm_base *wm, uint32_t serial) {
	(void)data;
	xdg_wm_base_pong(wm, serial);
}

static const struct xdg_wm_base_listener wm_listener = { .ping = wm_ping };

static void shm_format(void *data, struct wl_shm *s, uint32_t format) {
	(void)data;
	(void)s;
	(void)format;
}

static const struct wl_shm_listener shm_listener = { .format = shm_format };

static void xdg_surface_configure(void *data, struct xdg_surface *xs,
		uint32_t serial) {
	(void)data;
	xdg_surface_ack_configure(xs, serial);
	wl_surface_attach(surface, buffer, 0, 0);
	wl_surface_damage_buffer(surface, 0, 0, 300, 200);
	wl_surface_commit(surface);
	log_line("CONFIGURED", "");
}

static const struct xdg_surface_listener xdg_surface_listener = {
	.configure = xdg_surface_configure,
};

static void toplevel_configure(void *data, struct xdg_toplevel *t,
		int32_t w, int32_t h, struct wl_array *states) {
	(void)data;
	(void)t;
	(void)w;
	(void)h;
	(void)states;
}

static void toplevel_close(void *data, struct xdg_toplevel *t) {
	(void)data;
	(void)t;
}

static const struct xdg_toplevel_listener toplevel_listener = {
	.configure = toplevel_configure,
	.close = toplevel_close,
};

static void registry_global(void *data, struct wl_registry *reg, uint32_t name,
		const char *iface, uint32_t version) {
	(void)data;
	(void)version;
	if (strcmp(iface, "wl_compositor") == 0) {
		compositor = wl_registry_bind(reg, name, &wl_compositor_interface, 4);
	} else if (strcmp(iface, "wl_shm") == 0) {
		shm = wl_registry_bind(reg, name, &wl_shm_interface, 1);
		wl_shm_add_listener(shm, &shm_listener, NULL);
	} else if (strcmp(iface, "wl_seat") == 0) {
		seat = wl_registry_bind(reg, name, &wl_seat_interface, 5);
		wl_seat_add_listener(seat, &seat_listener, NULL);
	} else if (strcmp(iface, "xdg_wm_base") == 0) {
		wm_base = wl_registry_bind(reg, name, &xdg_wm_base_interface, 2);
		xdg_wm_base_add_listener(wm_base, &wm_listener, NULL);
	} else if (strcmp(iface, "zwp_text_input_manager_v3") == 0) {
		ti_mgr = wl_registry_bind(reg, name, &zwp_text_input_manager_v3_interface, 1);
	}
}

static void registry_remove(void *data, struct wl_registry *reg, uint32_t name) {
	(void)data;
	(void)reg;
	(void)name;
}

static const struct wl_registry_listener registry_listener = {
	.global = registry_global,
	.global_remove = registry_remove,
};

static struct wl_buffer *create_buffer(void) {
	int fd = memfd_create("ime-client", MFD_CLOEXEC);
	if (fd < 0) {
		perror("memfd_create");
		exit(1);
	}
	int stride = 300 * 4;
	int size = stride * 200;
	if (ftruncate(fd, size) < 0) {
		perror("ftruncate");
		exit(1);
	}
	void *data = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
	if (data == MAP_FAILED) {
		perror("mmap");
		exit(1);
	}
	uint32_t *px = data;
	for (int i = 0; i < 300 * 200; i++) {
		px[i] = 0xffffffff;
	}
	struct wl_shm_pool *pool = wl_shm_create_pool(shm, fd, size);
	struct wl_buffer *buf = wl_shm_pool_create_buffer(pool, 0, 300, 200,
			stride, WL_SHM_FORMAT_XRGB8888);
	wl_shm_pool_destroy(pool);
	close(fd);
	return buf;
}

int main(int argc, char **argv) {
	const char *title = argc > 1 ? argv[1] : "ime-test";
	display = wl_display_connect(NULL);
	if (display == NULL) {
		fprintf(stderr, "连接 wayland 失败\n");
		return 1;
	}
	struct wl_registry *registry = wl_display_get_registry(display);
	wl_registry_add_listener(registry, &registry_listener, NULL);
	wl_display_roundtrip(display);
	if (compositor == NULL || shm == NULL || wm_base == NULL || seat == NULL ||
			ti_mgr == NULL) {
		fprintf(stderr, "缺少必需全局\n");
		return 1;
	}
	buffer = create_buffer();
	surface = wl_compositor_create_surface(compositor);
	xdg_surface = xdg_wm_base_get_xdg_surface(wm_base, surface);
	xdg_surface_add_listener(xdg_surface, &xdg_surface_listener, NULL);
	toplevel = xdg_surface_get_toplevel(xdg_surface);
	xdg_toplevel_add_listener(toplevel, &toplevel_listener, NULL);
	xdg_toplevel_set_title(toplevel, title);
	wl_surface_commit(surface);
	ti = zwp_text_input_manager_v3_get_text_input(ti_mgr, seat);
	zwp_text_input_v3_add_listener(ti, &ti_listener, NULL);
	log_line("READY", "");
	while (wl_display_dispatch(display) != -1) {
	}
	return 0;
}
