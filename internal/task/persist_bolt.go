package task

import (
	"encoding/json"
	"os"
	"path/filepath"

	bolt "go.etcd.io/bbolt"

	"github.com/mixumos/mixumos/internal/wire"
)

// BoltPersist 把任务持久化到 bolt，容器重启后任务历史仍在。
type BoltPersist struct {
	db *bolt.DB
}

var bktTasks = []byte("tasks")

// OpenPersist 打开任务库。
func OpenPersist(dir string) (*BoltPersist, error) {
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return nil, err
	}
	db, err := bolt.Open(filepath.Join(dir, "tasks.db"), 0o600, nil)
	if err != nil {
		return nil, err
	}
	err = db.Update(func(tx *bolt.Tx) error {
		_, err := tx.CreateBucketIfNotExists(bktTasks)
		return err
	})
	if err != nil {
		db.Close()
		return nil, err
	}
	return &BoltPersist{db: db}, nil
}

// Close 关闭。
func (p *BoltPersist) Close() error { return p.db.Close() }

// Save 保存任务。
func (p *BoltPersist) Save(t *wire.Task) error {
	return p.db.Update(func(tx *bolt.Tx) error {
		raw, err := json.Marshal(t)
		if err != nil {
			return err
		}
		return tx.Bucket(bktTasks).Put([]byte(t.ID), raw)
	})
}

// List 列出全部任务。
func (p *BoltPersist) List() ([]*wire.Task, error) {
	var out []*wire.Task
	err := p.db.View(func(tx *bolt.Tx) error {
		return tx.Bucket(bktTasks).ForEach(func(_, v []byte) error {
			t := &wire.Task{}
			if err := json.Unmarshal(v, t); err != nil {
				return nil // 跳过坏记录
			}
			out = append(out, t)
			return nil
		})
	})
	return out, err
}
