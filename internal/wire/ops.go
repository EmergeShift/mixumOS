package wire

// Op 名常量。服务端回复在对应 op 后追加 ".ok" / ".err"。
const (
	OpHello = "hello"

	OpTaskSubmit = "task.submit"
	OpTaskList   = "task.list"
	OpTaskGet    = "task.get"
	OpTaskCancel = "task.cancel"
	OpTaskEvents = "task.events" // 订阅事件流
	OpTaskEvent  = "task.event"  // 服务端推送
	OpTaskAttach = "task.attach" // 附加到任务 stdio

	OpSnapCreate  = "snap.create"
	OpSnapTree    = "snap.tree"
	OpSnapRestore = "snap.restore"
	OpSnapFork    = "snap.fork"
	OpSnapDelete  = "snap.delete"
	OpSnapDiff    = "snap.diff"
	OpSnapPush    = "snap.push"  // 向远端推送节点
	OpSnapPull    = "snap.pull"  // 从远端拉取节点
	OpSnapGc      = "snap.gc"
	OpSnapChunk   = "snap.chunk" // 块传输（请求/应答走二进制通道）

	OpDisplayList    = "display.list"
	OpDisplayAttach  = "display.attach"
	OpDisplayDetach  = "display.detach"
	OpDisplayWindows = "display.windows" // 服务端推送：窗口增删/标题变化
	OpDisplayInput   = "display.input"
	OpDisplayTakeover = "display.takeover" // claim | release

	OpSysInfo = "sys.info"
	OpSysPing = "sys.ping"
)

// ErrCode 是 *.err 负载中的稳定错误码，客户端据此做程序化处理。
type ErrCode string

const (
	ErrBadRequest      ErrCode = "bad_request"
	ErrUnauthorized    ErrCode = "unauthorized"
	ErrForbidden       ErrCode = "forbidden"       // 能力令牌缺少所需 scope
	ErrNotFound        ErrCode = "not_found"
	ErrConflict        ErrCode = "conflict"        // 例如接管冲突、重放 jti
	ErrReplayDetected  ErrCode = "replay_detected"
	ErrBusy            ErrCode = "busy"
	ErrInternal        ErrCode = "internal"
	ErrUnsupported     ErrCode = "unsupported"
	ErrRateLimited     ErrCode = "rate_limited"
	ErrSnapshotInvalid ErrCode = "snapshot_invalid" // 内核/架构不匹配等
)

// Cap 是能力令牌中的 scope。令牌只授最小集合。
type Cap string

const (
	CapTaskRun     Cap = "task.run"
	CapTaskObserve Cap = "task.observe"
	CapSnapManage  Cap = "snap.manage"
	CapSnapSync    Cap = "snap.sync"
	CapDisplayView Cap = "display.view"
	CapDisplayInput Cap = "display.input"
	CapAdmin       Cap = "admin"
)
