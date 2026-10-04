// Package snap 实现 mixumOS 的快照引擎：内容寻址块存储、文件清单、
// 快照树 DAG（git 式分支/回退/fork）、磁盘与内存快照、跨设备块同步。
// 设计见 docs/snapshots.md。
package snap

import (
	crand "crypto/rand"
	"crypto/aes"
	"crypto/cipher"
	"crypto/hkdf"
	"crypto/sha256"
	"errors"
	"fmt"
	"hash"
	"io"
	"os"
	"path/filepath"
	"strings"
	"sync"

	"github.com/klauspost/compress/zstd"
)

// ChunkSize 是清单分块的标称大小。最后一个块可小于此值。
const ChunkSize = 1 << 20 // 1 MiB

// Store 是内容寻址块仓库。块以 zstd 压缩落盘，
// 可选 AES-256-GCM 静态加密（密钥来自 master key 的 HKDF 派生，域分离自存储 id）。
// 块哈希 = 明文数据的 SHA-256，与是否加密无关，因此跨设备去重不受加密影响。
type Store struct {
	dir      string
	zenc     *zstd.Encoder
	zdec     *zstd.Decoder
	aead     cipher.AEAD // 为 nil 时不加密
	storeKey []byte      // 派生用主密钥（未加载则不加密）
	storeID  string

	mu      sync.Mutex
	gcMarks map[string]struct{}
}

// OpenStore 打开（必要时初始化）一个块仓库。
// masterKey 非空时启用静态加密；密钥文件由调用方（engine）管理。
func OpenStore(dir string, masterKey []byte, storeID string) (*Store, error) {
	if err := os.MkdirAll(filepath.Join(dir, "objects"), 0o700); err != nil {
		return nil, err
	}
	s := &Store{dir: dir, gcMarks: map[string]struct{}{}, storeID: storeID}
	var err error
	s.zenc, err = zstd.NewWriter(nil, zstd.WithEncoderLevel(zstd.SpeedFastest))
	if err != nil {
		return nil, err
	}
	s.zdec, err = zstd.NewReader(nil)
	if err != nil {
		return nil, err
	}
	if len(masterKey) > 0 {
		s.storeKey = masterKey
		// HKDF 派生：info 绑定用途与存储 id，避免跨仓库重用密钥流。
		key, err := hkdf.Key(sha256.New, masterKey, []byte("mixum/snap/v1:"+storeID), "chunk-aead", 32)
		if err != nil {
			return nil, err
		}
		block, err := aes.NewCipher(key)
		if err != nil {
			return nil, err
		}
		aead, err := cipher.NewGCM(block)
		if err != nil {
			return nil, err
		}
		s.aead = aead
	}
	return s, nil
}

// Hash 计算块内容哈希（明文）。
func Hash(r io.Reader) (string, int64, error) {
	h := sha256.New()
	n, err := io.Copy(h, r)
	if err != nil {
		return "", 0, err
	}
	return hexHash(h), n, nil
}

func hexHash(h hash.Hash) string { return fmt.Sprintf("%x", h.Sum(nil)) }

// chunkPath 块文件路径：objects/ab/abcdef…
func chunkPath(dir, hash string) string {
	if len(hash) < 3 {
		return filepath.Join(dir, "objects", hash)
	}
	return filepath.Join(dir, "objects", hash[:2], hash[2:])
}

// Has 判断块是否已存在。
func (s *Store) Has(hashStr string) bool {
	_, err := os.Stat(chunkPath(s.dir, hashStr))
	return err == nil
}

// Put 写入一个明文块，返回其哈希。加密与压缩在此完成。
func (s *Store) Put(plain []byte) (string, error) {
	sum := sha256.Sum256(plain)
	hashStr := hexHashHash(sum)
	path := chunkPath(s.dir, hashStr)
	if _, err := os.Stat(path); err == nil {
		return hashStr, nil // 内容寻址：已存在即幂等
	}
	blob := s.zenc.EncodeAll(plain, nil)
	if s.aead != nil {
		nonce := make([]byte, s.aead.NonceSize())
		if _, err := randRead(nonce); err != nil {
			return "", err
		}
		sealed := s.aead.Seal(nil, nonce, blob, domainSeparation(s.storeID, hashStr))
		blob = append(nonce, sealed...)
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return "", err
	}
	tmp := path + ".tmp." + fmt.Sprintf("%d", os.Getpid())
	if err := os.WriteFile(tmp, blob, 0o600); err != nil {
		return "", err
	}
	if err := os.Rename(tmp, path); err != nil {
		os.Remove(tmp)
		return "", err
	}
	return hashStr, nil
}

// Get 读取一个块并解压（解密）。
func (s *Store) Get(hashStr string) ([]byte, error) {
	blob, err := os.ReadFile(chunkPath(s.dir, hashStr))
	if err != nil {
		return nil, err
	}
	if s.aead != nil {
		ns := s.aead.NonceSize()
		if len(blob) < ns {
			return nil, errors.New("snap: 加密块被截断")
		}
		blob, err = s.aead.Open(nil, blob[:ns], blob[ns:], domainSeparation(s.storeID, hashStr))
		if err != nil {
			return nil, fmt.Errorf("snap: 块解密失败（密钥不匹配或损坏）: %w", err)
		}
	}
	out, err := s.zdec.DecodeAll(blob, nil)
	if err != nil {
		return nil, fmt.Errorf("snap: 块解压失败: %w", err)
	}
	// 完整性：明文哈希必须等于块名。
	sum := sha256.Sum256(out)
	if hexHashHash(sum) != hashStr {
		return nil, errors.New("snap: 块哈希校验失败")
	}
	return out, nil
}

// Mark / Sweep 支持 GC：先 Mark 所有存活块，再 Sweep 删除未标记块。
func (s *Store) Mark(hashStr string) {
	s.mu.Lock()
	s.gcMarks[hashStr] = struct{}{}
	s.mu.Unlock()
}

func (s *Store) Sweep() (removed int, err error) {
	s.mu.Lock()
	marks := s.gcMarks
	s.gcMarks = map[string]struct{}{}
	s.mu.Unlock()
	objectsDir := filepath.Join(s.dir, "objects")
	entries, err := os.ReadDir(objectsDir)
	if err != nil {
		return 0, err
	}
	for _, lvl := range entries {
		if !lvl.IsDir() {
			continue
		}
		files, err := os.ReadDir(filepath.Join(objectsDir, lvl.Name()))
		if err != nil {
			continue
		}
		for _, f := range files {
			name := lvl.Name() + f.Name()
			if _, ok := marks[name]; !ok {
				os.Remove(filepath.Join(objectsDir, lvl.Name(), f.Name()))
				removed++
			}
		}
	}
	return removed, nil
}

// Count 返回块总数（用于统计）。
func (s *Store) Count() int {
	n := 0
	objectsDir := filepath.Join(s.dir, "objects")
	lvls, err := os.ReadDir(objectsDir)
	if err != nil {
		return 0
	}
	for _, lvl := range lvls {
		if !lvl.IsDir() {
			continue
		}
		files, _ := os.ReadDir(filepath.Join(objectsDir, lvl.Name()))
		n += len(files)
	}
	return n
}

func domainSeparation(storeID, chunkHash string) []byte {
	return []byte("mixum-chunk:" + storeID + ":" + chunkHash)
}

func hexHashHash(sum [32]byte) string {
	const hexdigits = "0123456789abcdef"
	out := make([]byte, 64)
	for i, b := range sum {
		out[i*2] = hexdigits[b>>4]
		out[i*2+1] = hexdigits[b&0x0f]
	}
	return string(out)
}

func randRead(b []byte) (int, error) {
	return crand.Read(b)
}

// splitChunks 把一个 reader 切成 ChunkSize 块。
func splitChunks(r io.Reader, emit func([]byte) error) error {
	buf := make([]byte, ChunkSize)
	for {
		n, err := io.ReadFull(r, buf)
		if n > 0 {
			if err := emit(buf[:n]); err != nil {
				return err
			}
		}
		if err == io.EOF || err == io.ErrUnexpectedEOF {
			return nil
		}
		if err != nil {
			return err
		}
	}
}

// sanitizeRelPath 防路径穿越：清单里的相对路径必须安全。
func sanitizeRelPath(p string) (string, error) {
	clean := filepath.Clean("/" + strings.TrimPrefix(p, "/"))
	if strings.Contains(clean, "..") {
		return "", fmt.Errorf("snap: 清单路径可疑: %q", p)
	}
	return strings.TrimPrefix(clean, "/"), nil
}
