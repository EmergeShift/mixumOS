// Package task 实现 mixumOS 的任务引擎：任务生命周期、事件总线、
// AgentDriver 插件接口与内置驱动。外部 agent 通过 external 驱动接入，
// 协议见 docs/agent-bridge.md。
package task

import (
	"context"
	"fmt"
	"sync"
	"time"

	"github.com/mixumos/mixumos/internal/wire"
)

// 任务状态机。
const (
	StateQueued    = "queued"
	StateRunning   = "running"
	StateSuspended = "suspended"
	StateCompleted = "completed"
	StateFailed    = "failed"
	StateCanceled  = "canceled"
)

// Task 是引擎内部的任务对象。
type Task struct {
	wire.Task
	cancel context.CancelFunc
	mu     sync.Mutex

	// pgroups 记录本任务启动的进程组，供快照冻结/接管/终止使用。
	pgroups   map[int]struct{}
	pgroupsMu sync.Mutex
}

func (t *Task) addPgid(pgid int) {
	t.pgroupsMu.Lock()
	t.pgroups[pgid] = struct{}{}
	t.pgroupsMu.Unlock()
}

// Pgids 返回任务进程组列表（快照冻结用）。
func (t *Task) Pgids() []int {
	t.pgroupsMu.Lock()
	defer t.pgroupsMu.Unlock()
	out := make([]int, 0, len(t.pgroups))
	for p := range t.pgroups {
		out = append(out, p)
	}
	return out
}

// Sandbox 是驱动眼中的容器环境。
type Sandbox interface {
	// Exec 运行一个进程并流式回传输出；返回退出码。
	Exec(ctx context.Context, name string, args []string, env []string, dir string, onOut func(line []byte)) (int, error)
	// Spawn 启动一个长驻进程（如浏览器），不等待退出。
	Spawn(ctx context.Context, name string, args []string, env []string, dir string) (int, error)
	// SetTaskPid 注册任务首领进程（内存快照目标）。
	SetTaskPid(pid int)
	// Signal 向任务进程组发信号（接管时暂停/恢复 agent 动作）。
	Signal(sig string) error
}

// Event 是任务事件的内部表示。
type Event struct {
	Type string // state|log|step|progress|error|window
	Step string
	Data any
}

// Driver 是 agent 的插件接口。实现方只需关心 Run；
// 事件通过 emit 上报，Sandbox 提供执行环境。
type Driver interface {
	Name() string
	Run(ctx context.Context, t *Task, sb Sandbox, emit func(Event)) error
}

// OptionalSuspender 允许驱动支持暂停（用于用户接管）。
type OptionalSuspender interface {
	Suspend(t *Task) error
	Resume(t *Task) error
}

// Registry 是驱动注册表。新 agent 在 main 里 Register 即接入。
type Registry struct {
	mu      sync.RWMutex
	drivers map[string]Driver
}

func NewRegistry() *Registry { return &Registry{drivers: map[string]Driver{}} }

// Register 注册驱动；重名返回错误。
func (r *Registry) Register(d Driver) error {
	r.mu.Lock()
	defer r.mu.Unlock()
	if _, dup := r.drivers[d.Name()]; dup {
		return fmt.Errorf("task: 驱动重名: %s", d.Name())
	}
	r.drivers[d.Name()] = d
	return nil
}

// Lookup 查找驱动，name 为空返回默认。
func (r *Registry) Lookup(name string) (Driver, error) {
	r.mu.RLock()
	defer r.mu.RUnlock()
	if name == "" {
		if d, ok := r.drivers["shell"]; ok {
			return d, nil
		}
	}
	if d, ok := r.drivers[name]; ok {
		return d, nil
	}
	return nil, fmt.Errorf("task: 未注册的驱动: %q", name)
}

// Names 列出全部驱动名。
func (r *Registry) Names() []string {
	r.mu.RLock()
	defer r.mu.RUnlock()
	out := make([]string, 0, len(r.drivers))
	for n := range r.drivers {
		out = append(out, n)
	}
	return out
}

// Engine 是任务引擎。
type Engine struct {
	reg *Registry
	sb  Sandbox

	mu      sync.Mutex
	tasks   map[string]*Task
	order   []string
	subs    map[chan wire.TaskEvent]struct{}
	persist Persist
}

// Persist 是任务持久化接口（bolt 实现）。
type Persist interface {
	Save(t *wire.Task) error
	List() ([]*wire.Task, error)
}

// NewEngine 构造引擎。
func NewEngine(reg *Registry, sb Sandbox, persist Persist) *Engine {
	return &Engine{
		reg:     reg,
		sb:      sb,
		tasks:   map[string]*Task{},
		subs:    map[chan wire.TaskEvent]struct{}{},
		persist: persist,
	}
}

// Submit 提交任务并异步执行。返回任务对象。
func (e *Engine) Submit(instruction, agent string, params map[string]string) (*Task, error) {
	drv, err := e.reg.Lookup(agent)
	if err != nil {
		return nil, err
	}
	t := &Task{
		Task: wire.Task{
			ID:          "task_" + wire.NewID(),
			Instruction: instruction,
			Agent:       drv.Name(),
			Runner:      "here",
			State:       StateQueued,
			Params:      params,
			CreatedAt:   time.Now().UnixMilli(),
		},
		pgroups: map[int]struct{}{},
	}
	e.mu.Lock()
	e.tasks[t.ID] = t
	e.order = append(e.order, t.ID)
	e.mu.Unlock()
	e.save(t)
	e.emit(t, Event{Type: "state", Data: StateQueued})

	ctx, cancel := context.WithCancel(context.Background())
	t.cancel = cancel
	go e.run(ctx, t, drv)
	return t, nil
}

func (e *Engine) run(ctx context.Context, t *Task, drv Driver) {
	e.setState(t, StateRunning)
	t.StartedAt = time.Now().UnixMilli()
	e.save(t)
	emit := func(ev Event) { e.emit(t, ev) }
	err := drv.Run(ctx, t, e.sb, emit)
	if ctx.Err() != nil {
		e.setState(t, StateCanceled)
		e.save(t)
		return
	}
	if err != nil {
		e.setState(t, StateFailed)
		t.Error = err.Error()
		e.emit(t, Event{Type: "error", Data: err.Error()})
		e.save(t)
		return
	}
	e.setState(t, StateCompleted)
	t.EndedAt = time.Now().UnixMilli()
	e.save(t)
}

func (e *Engine) setState(t *Task, s string) {
	t.mu.Lock()
	t.State = s
	t.mu.Unlock()
	e.emit(t, Event{Type: "state", Data: s})
}

// Cancel 取消任务。
func (e *Engine) Cancel(id string) error {
	e.mu.Lock()
	t := e.tasks[id]
	e.mu.Unlock()
	if t == nil {
		return fmt.Errorf("task: 任务不存在: %s", id)
	}
	if t.cancel != nil {
		t.cancel()
	}
	_ = e.sb.Signal("TERM")
	return nil
}

// Get 读取任务。
func (e *Engine) Get(id string) (*Task, error) {
	e.mu.Lock()
	defer e.mu.Unlock()
	t := e.tasks[id]
	if t == nil {
		return nil, fmt.Errorf("task: 任务不存在: %s", id)
	}
	return t, nil
}

// List 列出任务（按提交顺序）。
func (e *Engine) List() []*wire.Task {
	e.mu.Lock()
	defer e.mu.Unlock()
	out := make([]*wire.Task, 0, len(e.order))
	for _, id := range e.order {
		if t := e.tasks[id]; t != nil {
			out = append(out, &t.Task)
		}
	}
	return out
}

// Subscribe 订阅全部任务事件，返回通道与退订函数。
func (e *Engine) Subscribe() (<-chan wire.TaskEvent, func()) {
	ch := make(chan wire.TaskEvent, 256)
	e.mu.Lock()
	e.subs[ch] = struct{}{}
	e.mu.Unlock()
	return ch, func() {
		e.mu.Lock()
		delete(e.subs, ch)
		e.mu.Unlock()
	}
}

func (e *Engine) emit(t *Task, ev Event) {
	data, _ := marshalData(ev.Data)
	msg := wire.TaskEvent{
		TaskID: t.ID,
		Type:   ev.Type,
		Step:   ev.Step,
		Data:   data,
		TS:     time.Now().UnixMilli(),
	}
	e.mu.Lock()
	subs := make([]chan wire.TaskEvent, 0, len(e.subs))
	for ch := range e.subs {
		subs = append(subs, ch)
	}
	e.mu.Unlock()
	for _, ch := range subs {
		select {
		case ch <- msg:
		default: // 慢消费者丢事件，避免阻塞引擎
		}
	}
}

func (e *Engine) save(t *Task) {
	if e.persist == nil {
		return
	}
	t.mu.Lock()
	defer t.mu.Unlock()
	_ = e.persist.Save(&t.Task)
}
