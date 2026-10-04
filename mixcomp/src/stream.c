// stream.c —— 与 mixumd 的控制通道：JSON 控制消息 + JPEG 帧推送。
#include <errno.h>
#include <fcntl.h>
#include <json-c/json.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#include <drm_fourcc.h>
#include <jpeglib.h>

#include "mixcomp.h"

// 记录封装：4 字节小端长度 + 1 字节类型 + 负载（与 mixumd display.Broker 对齐）。
static bool rec_send(int fd, uint8_t type, const void *payload, size_t len) {
	if (fd < 0 || len > (16u << 20)) {
		return false;
	}
	uint8_t hdr[5] = {
		(uint8_t)(len & 0xff), (uint8_t)((len >> 8) & 0xff),
		(uint8_t)((len >> 16) & 0xff), (uint8_t)((len >> 24) & 0xff), type,
	};
	size_t total = sizeof(hdr) + len;
	uint8_t *buf = malloc(total);
	if (buf == NULL) {
		return false;
	}
	memcpy(buf, hdr, sizeof(hdr));
	memcpy(buf + sizeof(hdr), payload, len);
	bool ok = send(fd, buf, total, MSG_NOSIGNAL) == (ssize_t)total;
	free(buf);
	return ok;
}

bool stream_connect(struct mixcomp_server *s) {
	int fd = socket(AF_UNIX, SOCK_STREAM, 0);
	if (fd < 0) {
		return false;
	}
	struct sockaddr_un addr = {0};
	addr.sun_family = AF_UNIX;
	snprintf(addr.sun_path, sizeof(addr.sun_path), "%s", s->sock_path);
	if (connect(fd, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
		close(fd);
		return false;
	}
	int flags = fcntl(fd, F_GETFL, 0);
	fcntl(fd, F_SETFL, flags | O_NONBLOCK);
	s->sock_fd = fd;
	s->sock_ready = true;
	wlr_log(WLR_INFO, "已连接 mixumd 控制通道: %s", s->sock_path);
	return true;
}

// stream_flush —— 控制通道断开时轮询重连（由主循环定时调用）。
void stream_flush(struct mixcomp_server *s) {
	if (s->sock_ready) {
		return;
	}
	stream_connect(s);
}

void stream_send_window(struct mixcomp_server *s, struct mixcomp_window *w,
		const char *type) {
	if (!s->sock_ready) {
		return;
	}
	json_object *root = json_object_new_object();
	json_object_object_add(root, "type", json_object_new_string(type));
	json_object_object_add(root, "id", json_object_new_int(w->id));
	json_object_object_add(root, "appId", json_object_new_string(w->app_id));
	json_object_object_add(root, "title", json_object_new_string(w->title));
	json_object_object_add(root, "width", json_object_new_int(w->width));
	json_object_object_add(root, "height", json_object_new_int(w->height));
	const char *str = json_object_to_json_string(root);
	rec_send(s->sock_fd, REC_CONTROL, str, strlen(str));
	json_object_put(root);
}

// encode_jpeg —— 按 DRM 格式选择 libjpeg 扩展色彩空间。
static unsigned char *encode_jpeg(const unsigned char *pix, int stride,
		int width, int height, uint32_t drm_format, int quality,
		unsigned long *out_size) {
	J_COLOR_SPACE cs;
	switch (drm_format) {
	case DRM_FORMAT_ARGB8888:
	case DRM_FORMAT_XRGB8888:
		cs = JCS_EXT_BGRA; // 内存序 B,G,R,A
		break;
	case DRM_FORMAT_ABGR8888:
	case DRM_FORMAT_XBGR8888:
		cs = JCS_EXT_RGBA; // 内存序 R,G,B,A
		break;
	default:
		// 其余格式按 BGRA 容错（pixman 读回最常见的即 ARGB8888）。
		cs = JCS_EXT_BGRA;
		break;
	}
	struct jpeg_compress_struct cinfo;
	struct jpeg_error_mgr jerr;
	cinfo.err = jpeg_std_error(&jerr);
	jpeg_create_compress(&cinfo);
	unsigned char *out = NULL;
	*out_size = 0;
	jpeg_mem_dest(&cinfo, &out, out_size);
	cinfo.image_width = width;
	cinfo.image_height = height;
	cinfo.input_components = 4;
	cinfo.in_color_space = cs;
	jpeg_set_defaults(&cinfo);
	jpeg_set_quality(&cinfo, quality, TRUE);
	jpeg_start_compress(&cinfo, TRUE);
	while (cinfo.next_scanline < cinfo.image_height) {
		JSAMPROW row = (JSAMPROW)(pix + (size_t)cinfo.next_scanline * stride);
		jpeg_write_scanlines(&cinfo, &row, 1);
	}
	jpeg_finish_compress(&cinfo);
	jpeg_destroy_compress(&cinfo);
	return out;
}

void stream_send_frame(struct mixcomp_server *s, struct mixcomp_window *w,
		const unsigned char *rgba, int stride, int width, int height,
		uint32_t drm_format) {
	if (!s->sock_ready) {
		return;
	}
	unsigned long jpg_size = 0;
	unsigned char *jpg = encode_jpeg(rgba, stride, width, height, drm_format,
			s->jpeg_quality, &jpg_size);
	if (jpg == NULL || jpg_size == 0) {
		free(jpg);
		return;
	}
	uint8_t buf[FRAME_HDR];
	buf[0] = 0x01; // ChanVideo
	uint32_t id = w->id, wd = (uint32_t)width, ht = (uint32_t)height;
	uint32_t sz = (uint32_t)jpg_size;
	memcpy(buf + 1, &id, 4);
	memcpy(buf + 5, &wd, 4);
	memcpy(buf + 9, &ht, 4);
	memcpy(buf + 13, &sz, 4);
	uint16_t flags = FRAME_FLAG_KEYFRAME; // v1 全帧即关键帧
	if (w->have_cursor) {
		flags |= FRAME_FLAG_HASCURSOR;
	}
	memcpy(buf + 17, &flags, 2);
	int16_t cx = (int16_t)w->cursor_x, cy = (int16_t)w->cursor_y;
	uint16_t cw = 16, ch = 16;
	memcpy(buf + 19, &cx, 2);
	memcpy(buf + 21, &cy, 2);
	memcpy(buf + 23, &cw, 2);
	memcpy(buf + 25, &ch, 2);

	// 帧记录：头 + JPEG 一次发送。
	size_t total = 5 + FRAME_HDR + jpg_size;
	uint8_t *rec = malloc(total);
	if (rec != NULL) {
		uint32_t len = FRAME_HDR + (uint32_t)jpg_size;
		memcpy(rec, &len, 4);
		rec[4] = REC_VIDEO;
		memcpy(rec + 5, buf, FRAME_HDR);
		memcpy(rec + 5 + FRAME_HDR, jpg, jpg_size);
		if (send(s->sock_fd, rec, total, MSG_NOSIGNAL) < 0) {
			if (errno == EAGAIN || errno == EWOULDBLOCK) {
				// 拥塞：丢帧，实时性优先。
			} else {
				s->sock_ready = false;
				close(s->sock_fd);
				s->sock_fd = -1;
			}
		}
		free(rec);
	}
	free(jpg);
}

// recv_rec —— 非阻塞读一条记录；返回 1 有记录，0 无数据，-1 连接死亡。
static int recv_rec(int fd, uint8_t *type, uint8_t **payload, size_t *len) {
	uint8_t hdr[5];
	size_t got = 0;
	while (got < sizeof(hdr)) {
		ssize_t n = recv(fd, hdr + got, sizeof(hdr) - got, 0);
		if (n == 0) {
			return -1;
		}
		if (n < 0) {
			if (errno == EAGAIN || errno == EWOULDBLOCK) {
				return got > 0 ? -1 : 0; // 半条头绪按失败处理
			}
			return -1;
		}
		got += (size_t)n;
	}
	uint32_t blen = (uint32_t)hdr[0] | ((uint32_t)hdr[1] << 8) |
			((uint32_t)hdr[2] << 16) | ((uint32_t)hdr[3] << 24);
	if (blen > (16u << 20)) {
		return -1;
	}
	uint8_t *buf = malloc(blen ? blen : 1);
	size_t have = 0;
	while (have < blen) {
		ssize_t n = recv(fd, buf + have, blen - have, 0);
		if (n <= 0) {
			if (errno == EAGAIN || errno == EWOULDBLOCK) {
				usleep(1000);
				continue;
			}
			free(buf);
			return -1;
		}
		have += (size_t)n;
	}
	*type = hdr[4];
	*payload = buf;
	*len = blen;
	return 1;
}

static struct mixcomp_window *find_window(struct mixcomp_server *s, int id) {
	struct mixcomp_window *w;
	wl_list_for_each(w, &s->windows, link) {
		if ((int)w->id == id) {
			return w;
		}
	}
	return NULL;
}

// stream_poll_control —— 主循环里轮询 mixumd 的控制消息。
void stream_poll_control(struct mixcomp_server *s) {
	if (!s->sock_ready) {
		return;
	}
	for (int i = 0; i < 16; i++) { // 每轮最多处理 16 条，避免饿死捕获
		uint8_t type;
		uint8_t *payload;
		size_t len;
		int r = recv_rec(s->sock_fd, &type, &payload, &len);
		if (r <= 0) {
			if (r < 0) {
				s->sock_ready = false;
				close(s->sock_fd);
				s->sock_fd = -1;
				wlr_log(WLR_ERROR, "mixumd 控制通道断开，等待重连");
			}
			return;
		}
		if (type == REC_CONTROL) {
			json_object *root = json_tokener_parse((const char *)payload);
			if (root != NULL) {
				json_object *jt;
				if (json_object_object_get_ex(root, "type", &jt)) {
					const char *t = json_object_get_string(jt);
					if (strcmp(t, "stream.start") == 0 ||
							strcmp(t, "stream.stop") == 0) {
						json_object *jid, *jfps;
						int id = 0, fps = 30;
						if (json_object_object_get_ex(root, "id", &jid)) {
							id = json_object_get_int(jid);
						}
						if (strcmp(t, "stream.start") == 0 &&
								json_object_object_get_ex(root, "fps", &jfps)) {
							fps = json_object_get_int(jfps);
							if (fps <= 0 || fps > 60) {
								fps = 30;
							}
						}
						struct mixcomp_window *w = find_window(s, id);
						if (w != NULL) {
							w->want_stream = strcmp(t, "stream.start") == 0;
							w->fps = fps;
						}
					} else if (strcmp(t, "input") == 0 ||
							strcmp(t, "input.mode") == 0) {
						input_handle_json(s, (const char *)payload, len);
					}
				}
				json_object_put(root);
			}
		}
		free(payload);
	}
}
