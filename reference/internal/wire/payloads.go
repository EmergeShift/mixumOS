package wire

import "encoding/json"

// ---------- 握手 ----------

// HelloRequest 在 TLS 建链后发送，携带能力令牌。
type HelloRequest struct {
	Token   string   `json:"token"`            // 能力令牌（capability token）
	Client  string   `json:"client,omitempty"` // 客户端标识，如 "mixumctl/0.1"
	Device  string   `json:"device,omitempty"` // 设备名，如 "iphone-15"、"macbook-m3"
	Versions []string `json:"versions,omitempty"` // 客户端支持的协议版本
}

// HelloResponse 是握手应答。
type HelloResponse struct {
	SessionID string   `json:"sessionId"`
	Server    string   `json:"server"`    // "mixumd/0.1.0"
	Arch      string   `json:"arch"`      // 容器架构
	Kernel    string   `json:"kernel"`    // 容器内核版本
	Caps      []Cap    `json:"caps"`      // 令牌被授予的能力
	Versions  []int    `json:"versions"`  // 服务端支持的协议版本
	Heartbeat int      `json:"heartbeat"` // 建议心跳间隔秒
}

// ---------- 任务 ----------

// TaskSubmitRequest 提交一条指令。snapshotPolicy 控制任务前置无感快照。
type TaskSubmitRequest struct {
	Instruction string            `json:"instruction"`
	Agent       string            `json:"agent,omitempty"`  // agent 名，空为默认
	Runner      string            `json:"runner,omitempty"` // "here"（本容器）；预留 "cloud"
	Params      map[string]string `json:"params,omitempty"`
	IdempotencyKey string         `json:"idempotencyKey,omitempty"`
	SnapshotPolicy *SnapshotPolicy `json:"snapshotPolicy,omitempty"`
}

// SnapshotPolicy 描述任务的自动快照行为。
type SnapshotPolicy struct {
	Pre  bool `json:"pre"`            // 任务开始前打无感快照（默认 true）
	Kind string `json:"kind,omitempty"` // disk | memory | hybrid（默认 disk）
	OnFailure string `json:"onFailure,omitempty"` // none | restore（失败自动回退）
}

// Task 是任务的对象表示。
type Task struct {
	ID          string            `json:"id"`
	Instruction string            `json:"instruction"`
	Agent       string            `json:"agent"`
	Runner      string            `json:"runner"`
	State       string            `json:"state"` // queued|running|suspended|completed|failed|canceled
	PreSnapNode string            `json:"preSnapNode,omitempty"`
	Params      map[string]string `json:"params,omitempty"`
	Error       string            `json:"error,omitempty"`
	CreatedAt   int64             `json:"createdAt"`
	StartedAt   int64             `json:"startedAt,omitempty"`
	EndedAt     int64             `json:"endedAt,omitempty"`
}

// TaskEvent 是任务过程中的事件推送。
type TaskEvent struct {
	TaskID string          `json:"taskId"`
	Type   string          `json:"type"` // state|log|step|progress|error|window
	Step   string          `json:"step,omitempty"`
	Data   json.RawMessage `json:"data,omitempty"`
	TS     int64           `json:"ts"`
}

// ---------- 快照 ----------

// SnapCreateRequest 创建快照。auto 为 true 表示任务前置的无感快照。
type SnapCreateRequest struct {
	Kind   string `json:"kind"`             // disk | memory | hybrid
	Label  string `json:"label,omitempty"`
	Parent string `json:"parent,omitempty"` // 默认当前 HEAD
	Branch string `json:"branch,omitempty"` // 默认当前分支
	Auto   bool   `json:"auto,omitempty"`
}

// SnapNode 是快照树的一个节点。children 由服务端在 snap.tree 中填充。
type SnapNode struct {
	ID        string   `json:"id"`
	Parent    string   `json:"parent,omitempty"`
	Branch    string   `json:"branch"`
	Label     string   `json:"label,omitempty"`
	Kind      string   `json:"kind"`
	Auto      bool     `json:"auto,omitempty"`
	CreatedAt int64    `json:"createdAt"`
	Origin    string   `json:"origin,omitempty"` // 跨设备流转来源
	SizeBytes int64    `json:"sizeBytes"`
	Chunks    int      `json:"chunks"`
	Kernel    string   `json:"kernel,omitempty"`
	Arch      string   `json:"arch,omitempty"`
}

// SnapTreeResponse 返回整棵 DAG。
type SnapTreeResponse struct {
	Head   string     `json:"head"`
	Branch string     `json:"branch"`
	Nodes  []SnapNode `json:"nodes"`
}

// SnapRestoreRequest 恢复到某节点。strategy: inplace（当前分支回退）或 fork（从此节点叉出新分支继续）。
type SnapRestoreRequest struct {
	Node     string `json:"node"`
	Strategy string `json:"strategy,omitempty"` // inplace | fork，默认 inplace
	Label    string `json:"label,omitempty"`    // fork 时新分支名
}

// SnapRestoreResponse 恢复结果。fork 策略返回新分支名。
type SnapRestoreResponse struct {
	RestoredTo string `json:"restoredTo"`
	NewBranch  string `json:"newBranch,omitempty"`
	NewNode    string `json:"newNode,omitempty"` // 恢复后继续工作时产生的新节点挂靠点
}

// SnapForkRequest 从某节点分叉出新分支（不切换内容，仅建分支引用）。
type SnapForkRequest struct {
	Node   string `json:"node"`
	Branch string `json:"branch"`
}

// SnapDiffRequest 对比两个节点的差异。
type SnapDiffRequest struct {
	From string `json:"from"`
	To   string `json:"to"`
}

// SnapDiffResponse 返回文件级差异与块统计。
type SnapDiffResponse struct {
	Changed []string `json:"changed"`
	Added   []string `json:"added"`
	Removed []string `json:"removed"`
	ChunksNew int    `json:"chunksNew"`
	BytesNew  int64  `json:"bytesNew"`
}

// SnapPushRequest 把节点推送到远端 mixumd（跨设备流转）。
type SnapPushRequest struct {
	Node   string `json:"node"`
	Remote string `json:"remote"` // wss://host:port，远端须持有推送能力
	Token  string `json:"token,omitempty"` // 远端令牌
}

// SnapPushResponse 推送统计。
type SnapPushResponse struct {
	Node       string `json:"node"`
	ChunksSent int    `json:"chunksSent"`
	BytesSent  int64  `json:"bytesSent"`
	DedupRatio string `json:"dedupRatio"`
}

// ---------- 显示 ----------

// Window 描述容器内一个顶层窗口。
type Window struct {
	ID       uint32 `json:"id"`
	AppID    string `json:"appId"`
	Title    string `json:"title"`
	Width    int    `json:"width"`
	Height   int    `json:"height"`
	Focused  bool   `json:"focused"`
	Mode     string `json:"mode"` // agent（agent 持有输入）| user（用户接管）| free
}

// DisplayListResponse 列出当前窗口。
type DisplayListResponse struct {
	Windows []Window `json:"windows"`
}

// DisplayAttachRequest 订阅某窗口的实时流。
type DisplayAttachRequest struct {
	Window uint32 `json:"window"`
	FPS    int    `json:"fps,omitempty"`    // 期望帧率上限
	Quality string `json:"quality,omitempty"` // low|medium|high
}

// DisplayAttachResponse 附流应答。
type DisplayAttachResponse struct {
	Window    uint32 `json:"window"`
	Codec     string `json:"codec"` // v1: jpeg
	Width     int    `json:"width"`
	Height    int    `json:"height"`
	FPS       int    `json:"fps"`
}

// DisplayInputRequest 注入一条输入事件，仅在 user 模式且持有 display.input 能力时生效。
type DisplayInputRequest struct {
	Window uint32 `json:"window"`
	Type   string `json:"type"` // pointer_motion|pointer_button|pointer_axis|key|touch
	// pointer: x/y 为窗口内坐标（物理像素）
	X       float64 `json:"x,omitempty"`
	Y       float64 `json:"y,omitempty"`
	Button  string  `json:"button,omitempty"`  // left|right|middle
	Action  string  `json:"action,omitempty"`  // press|release|click|scroll_up|scroll_down
	Key     uint32  `json:"key,omitempty"`     // Linux 输入事件码（evdev）
	Keysym  string  `json:"keysym,omitempty"`  // 或用 XKB keysym 名，如 "a"、"Return"
	Mods    []string `json:"mods,omitempty"`   // ctrl|alt|shift|logo
}

// DisplayTakeoverRequest 请求接管或释放某窗口输入。
type DisplayTakeoverRequest struct {
	Window uint32 `json:"window"`
	Action string `json:"action"` // claim | release
	Force  bool   `json:"force,omitempty"` // 抢占 agent 的输入（agent 会收到让出事件）
}

// ---------- 系统 ----------

// SysInfoResponse 容器与守护进程信息。
type SysInfoResponse struct {
	Server     string   `json:"server"`
	Arch       string   `json:"arch"`
	Kernel     string   `json:"kernel"`
	OS         string   `json:"os"` // "Arch Linux"
	Hostname   string   `json:"hostname"`
	Caps       []Cap    `json:"caps"`
	SnapshotBackend string `json:"snapshotBackend"` // overlayfs|btrfs|directory
	CriuAvailable bool  `json:"criuAvailable"`
	Compositor bool     `json:"compositor"` // mixcomp 是否在线
	Windows    int      `json:"windows"`
}
