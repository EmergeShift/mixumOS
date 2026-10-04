/*
 * fakecomp —— 演示用假合成器:连接 mixumd 控制通道(unix socket),
 * 注册一个模拟窗口,按 ~10fps 推 JPEG 全帧(帧头带移动光标元数据),
 * 把收到的 display.input 事件画进画面(按键点亮格子、指针画点)。
 *
 * 用途:mixcomp 未就绪时给控制台窗口画布做端到端演示(见 console/README.md)。
 * 协议对齐 mixumd 控制通道:5 字节头(4 字节小端长度 + 1 字节类型),
 * 0x01 = JSON 控制消息(window.open / input …),0x02 = video 帧(协议 §5)。
 *
 * 编译(Alpine,镜像内含 build-base 与 libjpeg-turbo):
 *   gcc fakecomp.c -o fakecomp -ljpeg
 * 运行:
 *   XDG_RUNTIME_DIR=/run/mixum ./fakecomp [/run/mixum/display.sock]
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <unistd.h>
#include <time.h>
#include <math.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <jpeglib.h>

#define WIN_ID 9
#define W 640
#define H 400
#define FPS 10
#define MAX_KEYS 48

static int sock = -1;

static void die(const char *what) {
  perror(what);
  exit(1);
}

static void write_all(const void *buf, size_t n) {
  const uint8_t *p = buf;
  while (n > 0) {
    ssize_t k = write(sock, p, n);
    if (k <= 0) die("write");
    p += k;
    n -= (size_t)k;
  }
}

static void send_ctrl(const char *json) {
  uint32_t len = (uint32_t)strlen(json);
  uint8_t hdr[5] = {(uint8_t)(len & 0xff), (uint8_t)((len >> 8) & 0xff),
                    (uint8_t)((len >> 16) & 0xff), (uint8_t)((len >> 24) & 0xff), 0x01};
  write_all(hdr, 5);
  write_all(json, len);
}

static void send_frame(const uint8_t *jpeg, size_t len, int cx, int cy) {
  uint8_t hdr[5 + 27];
  uint32_t total = 27 + (uint32_t)len;
  hdr[0] = (uint8_t)(total & 0xff);
  hdr[1] = (uint8_t)((total >> 8) & 0xff);
  hdr[2] = (uint8_t)((total >> 16) & 0xff);
  hdr[3] = (uint8_t)((total >> 24) & 0xff);
  hdr[4] = 0x02;
  /* video 帧头 27 字节,小端(协议 §5) */
  hdr[5] = 0x01;
  uint32_t id = WIN_ID;
  memcpy(hdr + 6, &id, 4);
  uint32_t w = W, h = H, sz = (uint32_t)len;
  memcpy(hdr + 10, &w, 4);
  memcpy(hdr + 14, &h, 4);
  memcpy(hdr + 18, &sz, 4);
  uint16_t flags = 3; /* bit0 关键帧,bit1 携带光标 */
  memcpy(hdr + 22, &flags, 2);
  int16_t cx16 = (int16_t)cx, cy16 = (int16_t)cy;
  memcpy(hdr + 24, &cx16, 2);
  memcpy(hdr + 26, &cy16, 2);
  uint16_t cs = 16;
  memcpy(hdr + 28, &cs, 2);
  memcpy(hdr + 30, &cs, 2);
  write_all(hdr, sizeof(hdr));
  write_all(jpeg, len);
}

/* ---------- 画面 ---------- */

static uint8_t img[H * W * 3];

static void px(int x, int y, uint8_t r, uint8_t g, uint8_t b) {
  if (x < 0 || y < 0 || x >= W || y >= H) return;
  uint8_t *p = img + ((size_t)y * W + x) * 3;
  p[0] = r;
  p[1] = g;
  p[2] = b;
}

static void fill(int x0, int y0, int x1, int y1, uint8_t r, uint8_t g, uint8_t b) {
  for (int y = y0; y < y1; y++)
    for (int x = x0; x < x1; x++) px(x, y, r, g, b);
}

static void ring(int cx, int cy, int rad, uint8_t r, uint8_t g, uint8_t b) {
  for (int a = 0; a < 360; a += 2) {
    double t = a * 3.14159265 / 180.0;
    px(cx + (int)(rad * cos(t)), cy + (int)(rad * sin(t)), r, g, b);
  }
}

static void disk(int cx, int cy, int rad, uint8_t r, uint8_t g, uint8_t b) {
  for (int y = -rad; y <= rad; y++)
    for (int x = -rad; x <= rad; x++)
      if (x * x + y * y <= rad * rad) px(cx + x, cy + y, r, g, b);
}

/* ---------- 输入状态 ---------- */

static char keys[MAX_KEYS][20];
static int nkeys = 0;
static int ptr_x = -1, ptr_y = -1;
static long key_count = 0;

static void note_key(const char *sym) {
  if (strcmp(sym, "BackSpace") == 0 || strcmp(sym, "Delete") == 0) {
    if (nkeys > 0) nkeys--;
    return;
  }
  if (strcmp(sym, "Return") == 0 || strcmp(sym, "space") == 0) {
    if (nkeys < MAX_KEYS) snprintf(keys[nkeys++], 20, "__");
    return;
  }
  if (nkeys < MAX_KEYS) {
    snprintf(keys[nkeys], 20, "%s", sym);
    nkeys++;
  }
}

/* 从扁平 JSON 里取字符串字段(演示够用,不引解析器) */
static int json_str(const char *json, const char *field, char *out, size_t cap) {
  char pat[64];
  snprintf(pat, sizeof(pat), "\"%s\":\"", field);
  const char *p = strstr(json, pat);
  if (!p) return 0;
  p += strlen(pat);
  size_t i = 0;
  while (*p && *p != '"' && i + 1 < cap) out[i++] = *p++;
  out[i] = 0;
  return 1;
}

static void handle_ctrl(const char *json) {
  if (strstr(json, "\"type\":\"input\"")) {
    char kind[32] = "", sym[24] = "", action[24] = "";
    json_str(json, "kind", kind, sizeof(kind));
    json_str(json, "keysym", sym, sizeof(sym));
    json_str(json, "action", action, sizeof(action));
    if (strcmp(kind, "key") == 0 && strcmp(action, "press") == 0 && sym[0]) {
      note_key(sym);
      key_count++;
    }
    if (strcmp(kind, "pointer_motion") == 0) {
      const char *x = strstr(json, "\"x\":");
      const char *y = strstr(json, "\"y\":");
      if (x) ptr_x = (int)atof(x + 4);
      if (y) ptr_y = (int)atof(y + 4);
    }
    if (strcmp(kind, "pointer_button") == 0) key_count++; /* 点一下也点亮一格 */
    printf("input: kind=%s keysym=%s action=%s\n", kind, sym, action);
    fflush(stdout);
  }
}

/* ---------- JPEG 编码 ---------- */

static unsigned char *jpegbuf = NULL;

static size_t encode_jpeg(void) {
  struct jpeg_compress_struct cinfo;
  struct jpeg_error_mgr jerr;
  unsigned char *out = NULL;
  unsigned long size = 0;
  cinfo.err = jpeg_std_error(&jerr);
  jpeg_create_compress(&cinfo);
  jpeg_mem_dest(&cinfo, &out, &size); /* libjpeg 自行分配缓冲 */
  cinfo.image_width = W;
  cinfo.image_height = H;
  cinfo.input_components = 3;
  cinfo.in_color_space = JCS_RGB;
  jpeg_set_defaults(&cinfo);
  jpeg_set_quality(&cinfo, 70, TRUE);
  jpeg_start_compress(&cinfo, TRUE);
  while (cinfo.next_scanline < cinfo.image_height) {
    JSAMPROW row = img + (size_t)cinfo.next_scanline * W * 3;
    jpeg_write_scanlines(&cinfo, &row, 1);
  }
  jpeg_finish_compress(&cinfo);
  jpeg_destroy_compress(&cinfo);
  free(jpegbuf);
  jpegbuf = out;
  return size;
}

static void render(int cx, int cy, long tick) {
  fill(0, 0, W, H, 0x2b, 0x2b, 0x28);
  fill(0, 0, W, 36, 0x3a, 0x6e, 0xa5);                     /* 标题条 */
  fill(0, 36, W, 38, 0x22, 0x22, 0x20);
  fill(16, 60, W - 16, 62, 0x55, 0x55, 0x50);
  /* 输入格子:每个按键一格,格色随序号变化 */
  for (int i = 0; i < nkeys; i++) {
    int gx = 20 + (i % 12) * 50;
    int gy = 80 + (i / 12) * 60;
    uint8_t hue = (uint8_t)(40 + (i * 47) % 180);
    fill(gx, gy, gx + 42, gy + 42, hue, (uint8_t)(90 + (hue / 3)), 0x50);
    fill(gx + 2, gy + 2, gx + 40, gy + 40, 0x35, 0x35, 0x32);
    fill(gx + 2, gy + 2, gx + 2 + 36, gy + 4, hue, 0x8a, 0x60); /* 顶边亮条 */
  }
  /* 指针点(display.input pointer_motion) */
  if (ptr_x >= 0) disk(ptr_x, ptr_y, 6, 0xe0, 0xb0, 0x50);
  /* 空态提示:画一个呼吸方块 */
  if (nkeys == 0) {
    int a = (int)(8 + 6 * __builtin_sin(tick / 5.0));
    fill(W / 2 - a, 180, W / 2 + a, 180 + 2 * a, 0x50, 0x50, 0x4a);
  }
  ring(cx, cy, 10, 0xf4, 0xf4, 0xf2); /* 帧头光标位置的靶标:客户端箭头应落在环心 */
}

int main(int argc, char **argv) {
  const char *path = argc > 1 ? argv[1] : "/run/mixum/display.sock";

  struct sockaddr_un addr;
  memset(&addr, 0, sizeof(addr));
  addr.sun_family = AF_UNIX;
  snprintf(addr.sun_path, sizeof(addr.sun_path), "%s", path);
  sock = socket(AF_UNIX, SOCK_STREAM, 0);
  if (sock < 0) die("socket");
  if (connect(sock, (struct sockaddr *)&addr, sizeof(addr)) < 0) die("connect");

  char open_msg[128];
  snprintf(open_msg, sizeof(open_msg),
           "{\"type\":\"window.open\",\"id\":%d,\"appId\":\"demo\","
           "\"title\":\"模拟窗口演示(fakecomp)\",\"width\":%d,\"height\":%d,\"focused\":true}",
           WIN_ID, W, H);
  send_ctrl(open_msg);
  printf("fakecomp: 已注册窗口 #%d,推流中…\n", WIN_ID);
  fflush(stdout);

  /* 读线程:收控制消息(input 等)。简单起见用 fork 双进程。 */
  if (fork() == 0) {
    uint8_t lenb[5];
    for (;;) {
      size_t got = 0;
      while (got < 5) {
        ssize_t k = read(sock, lenb + got, 5 - got);
        if (k <= 0) _exit(0);
        got += (size_t)k;
      }
      uint32_t len = (uint32_t)lenb[0] | ((uint32_t)lenb[1] << 8) |
                     ((uint32_t)lenb[2] << 16) | ((uint32_t)lenb[3] << 24);
      if (lenb[4] != 0x01 || len == 0 || len > 1 << 20) continue;
      char *buf = malloc(len + 1);
      size_t have = 0;
      while (have < len) {
        ssize_t k = read(sock, buf + have, len - have);
        if (k <= 0) _exit(0);
        have += (size_t)k;
      }
      buf[len] = 0;
      handle_ctrl(buf);
      free(buf);
    }
  }

  /* 主循环:渲染 + 推帧 */
  long tick = 0;
  for (;;) {
    int cx = (int)(W / 2 + 220 * cos(tick / 9.0));
    int cy = (int)(H / 2 + 140 * sin(tick / 7.0));
    render(cx, cy, tick);
    size_t len = encode_jpeg();
    send_frame(jpegbuf, len, cx, cy);
    struct timespec ts = {0, 1000000000 / FPS};
    nanosleep(&ts, NULL);
    tick++;
  }
}
