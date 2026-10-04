// input.c —— 输入注入：内建 wayland 客户端 + virtual-pointer/keyboard 协议。
// mixcomp 连自己的 wayland 套接字，用 zwp_virtual_keyboard_v1 与
// zwlr_virtual_pointer_v1 创建虚拟设备；wlroots 为其生成标准输入事件，
// main.c 把事件路由到目标窗口的 seat。只依赖公开协议，行为与外部
// 输入工具（如 wtype/ydotool 的 wayland 路径）一致。
#include <errno.h>
#include <fcntl.h>
#include <json-c/json.h>
#include <linux/input-event-codes.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>
#include <wayland-client-core.h>
#include <wayland-client-protocol.h>
#include <wayland-client.h>
#include <xkbcommon/xkbcommon-names.h>
#include <xkbcommon/xkbcommon.h>

#include "mixcomp.h"

#define KEYMAP_FORMAT_XKB_V1 1
#define POINTER_AXIS_VERTICAL 0
#define AXIS_SOURCE_WHEEL 0
#define KEY_STATE_RELEASED 0
#define KEY_STATE_PRESSED 1
#define POINTER_ENTER 1

struct mixcomp_window;
static struct mixcomp_window *find_window(struct mixcomp_server *s, int id);

static struct mixcomp_window *find_window(struct mixcomp_server *s, int id) {
	struct mixcomp_window *w;
	wl_list_for_each(w, &s->windows, link) {
		if ((int)w->id == id) {
			return w;
		}
	}
	return NULL;
}

// ---------- 手工协议接口声明（与上游 XML 逐字段对齐） ----------

static const struct wl_message vp_requests[] = {
	{ "motion", "uff", NULL },
	{ "motion_absolute", "uuuuu", NULL },
	{ "button", "uuu", NULL },
	{ "axis", "uuf", NULL },
	{ "axis_source", "u", NULL },
	{ "axis_stop", "uu", NULL },
	{ "axis_discrete", "uui", NULL },
	{ "frame", "", NULL },
	{ "destroy", "", NULL },
};
static const struct wl_interface vp_interface = {
	"zwlr_virtual_pointer_v1", 2, 9, vp_requests, 0, NULL,
};

static const struct wl_message vp_manager_requests[] = {
	{ "create_virtual_pointer", "no", (const struct wl_interface *[]){ (struct wl_interface *)&wl_seat_interface, (struct wl_interface *)&vp_interface } },
	{ "create_virtual_pointer_with_output", "noo", (const struct wl_interface *[]){ (struct wl_interface *)&wl_seat_interface, NULL, (struct wl_interface *)&vp_interface } },
	{ "destroy", "", NULL },
};
static const struct wl_interface vp_manager_interface = {
	"zwlr_virtual_pointer_manager_v1", 2, 3, vp_manager_requests, 0, NULL,
};

static const struct wl_message kb_requests[] = {
	{ "keymap", "ihu", NULL },
	{ "key", "uuu", NULL },
	{ "modifiers", "uiiii", NULL },
	{ "destroy", "", NULL },
};
static const struct wl_interface kb_interface = {
	"zwp_virtual_keyboard_v1", 1, 4, kb_requests, 0, NULL,
};

static const struct wl_message kb_manager_requests[] = {
	{ "create_virtual_keyboard", "no", (const struct wl_interface *[]){ (struct wl_interface *)&wl_seat_interface, (struct wl_interface *)&kb_interface } },
	{ "destroy", "", NULL },
};
static const struct wl_interface kb_manager_interface = {
	"zwp_virtual_keyboard_manager_v1", 1, 2, kb_manager_requests, 0, NULL,
};

// create_virtual_pointer/keyboard 的 new_id 在上游 XML 中已标注接口，
// 因此 wl_proxy_marshal_flags 传目标接口；变参只有 seat。

// ---------- 客户端状态 ----------

struct inj_client {
	struct wl_display *display;
	struct wl_registry *registry;
	struct wl_seat *seat;
	struct wl_proxy *vp_mgr;
	struct wl_proxy *kb_mgr;
	struct wl_proxy *pointer;  // zwlr_virtual_pointer_v1
	struct wl_proxy *keyboard; // zwp_virtual_keyboard_v1
	struct xkb_context *xkb_ctx;
	struct xkb_keymap *xkb_keymap;
	struct xkb_state *xkb_state;
};

static struct inj_client inj = {0};

static uint32_t now_ms(void) {
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (uint32_t)((uint64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000);
}

static void seat_capabilities(void *data, struct wl_seat *seat, uint32_t caps) {
	(void)data; (void)seat; (void)caps; // 注入不需要 seat 能力协商
}

static const struct wl_seat_listener seat_listener = {
	.capabilities = seat_capabilities,
	.name = NULL,
};

static void registry_global(void *data, struct wl_registry *reg, uint32_t name,
		const char *iface, uint32_t version) {
	struct mixcomp_server *s = data;
	if (strcmp(iface, "wl_seat") == 0 && inj.seat == NULL) {
		inj.seat = wl_registry_bind(reg, name, &wl_seat_interface,
				version < 5 ? version : 5);
		wl_seat_add_listener(inj.seat, &seat_listener, s);
	} else if (strcmp(iface, "zwlr_virtual_pointer_manager_v1") == 0 && inj.vp_mgr == NULL) {
		uint32_t v = version < 2 ? version : 2;
		inj.vp_mgr = wl_registry_bind(reg, name, &vp_manager_interface, v);
	} else if (strcmp(iface, "zwp_virtual_keyboard_manager_v1") == 0 && inj.kb_mgr == NULL) {
		inj.kb_mgr = wl_registry_bind(reg, name, &kb_manager_interface, 1);
	}
}

static void registry_global_remove(void *data, struct wl_registry *reg, uint32_t name) {
	(void)data; (void)reg; (void)name;
}

static const struct wl_registry_listener registry_listener = {
	.global = registry_global,
	.global_remove = registry_global_remove,
};

// keymap_upload —— 生成默认 xkb keymap 并发送给虚拟键盘。
static bool keymap_upload(struct mixcomp_server *s) {
	if (inj.keyboard == NULL) {
		return false;
	}
	if (inj.xkb_ctx == NULL) {
		inj.xkb_ctx = xkb_context_new(XKB_CONTEXT_NO_FLAGS);
	}
	struct xkb_rule_names names = {
		.rules = "evdev", .model = "pc105", .layout = "us",
		.variant = "", .options = "",
	};
	inj.xkb_keymap = xkb_keymap_new_from_names(inj.xkb_ctx, &names, XKB_KEYMAP_COMPILE_NO_FLAGS);
	if (inj.xkb_keymap == NULL) {
		return false;
	}
	const char *str = xkb_keymap_get_as_string(inj.xkb_keymap, XKB_KEYMAP_FORMAT_TEXT_V1);
	size_t size = strlen(str) + 1;
	int fd = memfd_create("mixum-keymap", MFD_CLOEXEC);
	if (fd < 0) {
		return false;
	}
	if (ftruncate(fd, (off_t)size) != 0 || write(fd, str, size) != (ssize_t)size) {
		close(fd);
		return false;
	}
	lseek(fd, 0, SEEK_SET);
	// keymap(fd, size, format) —— fd 经 SCM_RIGHTS 传递，须在 flush 前有效。
	wl_proxy_marshal(inj.keyboard, 0, fd, (uint32_t)size, (uint32_t)KEYMAP_FORMAT_XKB_V1);
	close(fd); // libwayland 已在 sendmsg 时 dup
	inj.xkb_state = xkb_state_new(inj.xkb_keymap);
	wl_display_flush(s->inj_display);
	return inj.xkb_state != NULL;
}

// keycode_for_keysym —— 反查 keysym 对应的 evdev 码。
static uint32_t keycode_for_keysym(const char *name) {
	xkb_keysym_t sym = xkb_keysym_from_name(name, XKB_KEYSYM_CASE_INSENSITIVE);
	if (sym == XKB_KEY_NoSymbol || inj.xkb_keymap == NULL) {
		return 0;
	}
	xkb_keycode_t min = xkb_keymap_min_keycode(inj.xkb_keymap);
	xkb_keycode_t max = xkb_keymap_max_keycode(inj.xkb_keymap);
	for (xkb_keycode_t kc = min; kc <= max; kc++) {
		const xkb_keysym_t *syms;
		int n = xkb_keymap_key_get_syms_by_level(inj.xkb_keymap, kc, 0, 0, &syms);
		for (int i = 0; i < n; i++) {
			if (syms[i] == sym) {
				return (uint32_t)kc - 8; // xkb → evdev
			}
		}
	}
	return 0;
}

// send_modifiers —— 客户端侧 xkb 状态序列化后同步给键盘设备。
static void send_modifiers(struct mixcomp_server *s) {
	if (inj.keyboard == NULL || inj.xkb_state == NULL) {
		return;
	}
	uint32_t dep = xkb_state_serialize_mods(inj.xkb_state, XKB_STATE_MODS_DEPRESSED);
	uint32_t lat = xkb_state_serialize_mods(inj.xkb_state, XKB_STATE_MODS_LATCHED);
	uint32_t loc = xkb_state_serialize_mods(inj.xkb_state, XKB_STATE_MODS_LOCKED);
	uint32_t grp = xkb_state_serialize_layout(inj.xkb_state, XKB_STATE_LAYOUT_EFFECTIVE);
	wl_proxy_marshal(inj.keyboard, 2, now_ms(), (int32_t)dep, (int32_t)lat,
			(int32_t)loc, (int32_t)grp);
	wl_display_flush(s->inj_display);
}

static void key_event(struct mixcomp_server *s, uint32_t evdev_key, bool press) {
	if (inj.keyboard == NULL || inj.xkb_state == NULL) {
		return;
	}
	uint32_t t = now_ms();
	xkb_state_update_key(inj.xkb_state, (xkb_keycode_t)(evdev_key + 8),
			press ? XKB_KEY_DOWN : XKB_KEY_UP);
	wl_proxy_marshal(inj.keyboard, 1, t, evdev_key,
			(uint32_t)(press ? KEY_STATE_PRESSED : KEY_STATE_RELEASED));
	send_modifiers(s);
}

// ---------- 注入入口（mixumd 控制消息） ----------

static uint32_t mod_keycode(const char *m) {
	if (strcmp(m, "ctrl") == 0) {
		return KEY_LEFTCTRL;
	}
	if (strcmp(m, "alt") == 0) {
		return KEY_LEFTALT;
	}
	if (strcmp(m, "shift") == 0) {
		return KEY_LEFTSHIFT;
	}
	if (strcmp(m, "logo") == 0) {
		return KEY_LEFTMETA;
	}
	return 0;
}

static void inject_motion(struct mixcomp_server *s, struct mixcomp_window *w,
		double x, double y) {
	if (inj.pointer == NULL) {
		return;
	}
	s->focus_window = w; // 事件路由目标
	s->inj_width = w->width;
	s->inj_height = w->height;
	wl_proxy_marshal(inj.pointer, 1, now_ms(), (uint32_t)x, (uint32_t)y,
			(uint32_t)w->width, (uint32_t)w->height); // motion_absolute
	wl_proxy_marshal(inj.pointer, 7); // frame
	wl_display_flush(s->inj_display);
}

static void inject_button(struct mixcomp_server *s, struct mixcomp_window *w,
		const char *button, const char *action) {
	if (inj.pointer == NULL) {
		return;
	}
	s->focus_window = w;
	uint32_t code = strcmp(button, "right") == 0 ? BTN_RIGHT :
			strcmp(button, "middle") == 0 ? BTN_MIDDLE : BTN_LEFT;
	uint32_t t = now_ms();
	if (strcmp(action, "press") == 0 || strcmp(action, "click") == 0) {
		wl_proxy_marshal(inj.pointer, 2, t, code, (uint32_t)1);
		wl_proxy_marshal(inj.pointer, 7);
	}
	if (strcmp(action, "release") == 0 || strcmp(action, "click") == 0) {
		wl_proxy_marshal(inj.pointer, 2, t, code, (uint32_t)0);
		wl_proxy_marshal(inj.pointer, 7);
	}
	wl_display_flush(s->inj_display);
}

static void inject_axis(struct mixcomp_server *s, struct mixcomp_window *w,
		const char *action) {
	if (inj.pointer == NULL) {
		return;
	}
	s->focus_window = w;
	double delta = strcmp(action, "scroll_up") == 0 ? -1.0 : 1.0;
	uint32_t t = now_ms();
	wl_proxy_marshal(inj.pointer, 4, (uint32_t)AXIS_SOURCE_WHEEL);
	wl_proxy_marshal(inj.pointer, 3, t, (uint32_t)POINTER_AXIS_VERTICAL,
			wl_fixed_from_double(delta));
	wl_proxy_marshal(inj.pointer, 6, t, (uint32_t)POINTER_AXIS_VERTICAL,
			(int32_t)(delta * 10));
	wl_proxy_marshal(inj.pointer, 7);
	wl_display_flush(s->inj_display);
}

static void inject_key(struct mixcomp_server *s, struct mixcomp_window *w,
		const char *keysym_name, uint32_t raw_key, struct json_object *mods,
		const char *action) {
	if (inj.keyboard == NULL) {
		return;
	}
	s->focus_window = w;
	uint32_t key = raw_key;
	if (key == 0 && keysym_name != NULL) {
		key = keycode_for_keysym(keysym_name);
	}
	if (key == 0) {
		return;
	}
	bool press = strcmp(action, "press") == 0 || strcmp(action, "click") == 0;
	bool release = strcmp(action, "release") == 0 || strcmp(action, "click") == 0;

	// 修饰键：按住 → 敲键 → 松开。
	uint32_t mod_keys[4];
	int nmods = 0;
	if (mods != NULL && json_object_is_type(mods, json_type_array)) {
		size_t n = json_object_array_length(mods);
		for (size_t i = 0; i < n && nmods < 4; i++) {
			uint32_t mk = mod_keycode(json_object_get_string(json_object_array_get_idx(mods, i)));
			if (mk) {
				mod_keys[nmods++] = mk;
			}
		}
	}
	bool click = press && release;
	if (click) {
		for (int i = 0; i < nmods; i++) {
			key_event(s, mod_keys[i], true);
		}
		key_event(s, key, true);
		key_event(s, key, false);
		for (int i = nmods - 1; i >= 0; i--) {
			key_event(s, mod_keys[i], false);
		}
	} else {
		if (press) {
			key_event(s, key, true);
		}
		if (release) {
			key_event(s, key, false);
		}
	}
}

void input_handle_json(struct mixcomp_server *s, const char *json, size_t len) {
	(void)len;
	json_object *root = json_tokener_parse(json);
	if (root == NULL) {
		return;
	}
	json_object *jt = NULL, *jid = NULL, *jkind = NULL;
	json_object_object_get_ex(root, "type", &jt);
	json_object_object_get_ex(root, "id", &jid);
	json_object_object_get_ex(root, "kind", &jkind);
	const char *type = jt ? json_object_get_string(jt) : "";
	int id = jid ? json_object_get_int(jid) : 0;
	const char *kind = jkind ? json_object_get_string(jkind) : "";

	if (strcmp(type, "input.mode") == 0) {
		// 接管模式变化：v1 由 mixumd 仲裁，合成器仅记录。
		json_object_put(root);
		return;
	}
	struct mixcomp_window *w = find_window(s, id);
	if (w == NULL || strcmp(type, "input") != 0) {
		json_object_put(root);
		return;
	}
	json_object *jx = NULL, *jy = NULL, *jbtn = NULL, *jact = NULL;
	json_object *jkey = NULL, *jksym = NULL, *jmods = NULL;
	json_object_object_get_ex(root, "x", &jx);
	json_object_object_get_ex(root, "y", &jy);
	json_object_object_get_ex(root, "button", &jbtn);
	json_object_object_get_ex(root, "action", &jact);
	json_object_object_get_ex(root, "key", &jkey);
	json_object_object_get_ex(root, "keysym", &jksym);
	json_object_object_get_ex(root, "mods", &jmods);

	if (strcmp(kind, "pointer_motion") == 0) {
		inject_motion(s, w, jx ? json_object_get_double(jx) : 0,
				jy ? json_object_get_double(jy) : 0);
	} else if (strcmp(kind, "pointer_button") == 0) {
		inject_button(s, w, jbtn ? json_object_get_string(jbtn) : "left",
				jact ? json_object_get_string(jact) : "click");
	} else if (strcmp(kind, "pointer_axis") == 0) {
		inject_axis(s, w, jact ? json_object_get_string(jact) : "scroll_down");
	} else if (strcmp(kind, "key") == 0) {
		uint32_t key = jkey ? (uint32_t)json_object_get_int(jkey) : 0;
		inject_key(s, w, jksym ? json_object_get_string(jksym) : NULL, key,
				jmods, jact ? json_object_get_string(jact) : "click");
	}
	json_object_put(root);
}

// ---------- 初始化与事件循环集成 ----------

static int inj_fd_cb(int fd, uint32_t mask, void *data) {
	(void)fd;
	struct mixcomp_server *s = data;
	if (mask & WL_EVENT_READABLE) {
		wl_display_dispatch(inj.display);
	}
	if (mask & (WL_EVENT_HANGUP | WL_EVENT_ERROR)) {
		wlr_log(WLR_ERROR, "注入客户端连接断开");
		return -1;
	}
	wl_display_flush(s->inj_display);
	return 0;
}

bool input_init(struct mixcomp_server *s) {
	inj.display = wl_display_connect(NULL); // WAYLAND_DISPLAY 已由 main 设置
	if (inj.display == NULL) {
		wlr_log(WLR_ERROR, "注入客户端无法连接自身 wayland 套接字");
		return false;
	}
	s->inj_display = inj.display;
	inj.registry = wl_display_get_registry(inj.display);
	wl_registry_add_listener(inj.registry, &registry_listener, s);
	wl_display_roundtrip(inj.display);
	wl_display_roundtrip(inj.display); // 第二轮确保 managers 就绪

	if (inj.vp_mgr != NULL) {
		inj.pointer = wl_proxy_marshal_flags(inj.vp_mgr, 0, &vp_interface, 2, 0,
				inj.seat);
	}
	if (inj.kb_mgr != NULL) {
		inj.keyboard = wl_proxy_marshal_flags(inj.kb_mgr, 0, &kb_interface, 1, 0,
				inj.seat);
	}
	if (inj.pointer == NULL || inj.keyboard == NULL) {
		wlr_log(WLR_ERROR, "虚拟输入设备创建失败（pointer=%p keyboard=%p）",
				(void *)inj.pointer, (void *)inj.keyboard);
		return false;
	}
	if (!keymap_upload(s)) {
		wlr_log(WLR_ERROR, "keymap 上传失败，键盘注入不可用");
	}
	wl_display_flush(inj.display);

	int fd = wl_display_get_fd(inj.display);
	struct wl_event_loop *loop = wl_display_get_event_loop(s->display);
	s->inj_src = wl_event_loop_add_fd(loop, fd, WL_EVENT_READABLE, inj_fd_cb, s);
	wlr_log(WLR_INFO, "注入客户端就绪：virtual pointer + keyboard 已创建");
	return true;
}
