package cryonet

import (
	"sync"
	"time"
	"weak"

	goakt "github.com/tochemey/goakt/v3/actor"
)

type TTLMap struct {
	timeout time.Duration

	lock sync.RWMutex
	m    map[string][]*item
}

type item struct {
	value *goakt.PID
}

func NewTTLMap(timeout time.Duration) *TTLMap {
	return &TTLMap{
		timeout: timeout,
		m:       make(map[string][]*item),
	}
}

func (t *TTLMap) Add(key string, value *goakt.PID) {
	t.lock.Lock()
	defer t.lock.Unlock()
	if t.m == nil {
		t.m = make(map[string][]*item)
	}
	item := &item{value: value}
	t.m[key] = append(t.m[key], item)

	timeout := t.timeout
	weakT := weak.Make(t)
	go func() {
		<-time.After(timeout)
		t := weakT.Value()
		if t == nil {
			return
		}
		t.lock.Lock()
		defer t.lock.Unlock()
		items := t.m[key]
		for i, it := range items {
			if it == item {
				t.m[key] = append(items[:i], items[i+1:]...)
				break
			}
		}
	}()
}

func (t *TTLMap) GetNewest(key string) *goakt.PID {
	t.lock.RLock()
	defer t.lock.RUnlock()
	items, ok := t.m[key]
	if !ok {
		return nil
	}
	for i := len(items) - 1; i >= 0; i-- {
		if items[i].value != nil && items[i].value.IsRunning() {
			return items[i].value
		}
	}
	return nil
}
