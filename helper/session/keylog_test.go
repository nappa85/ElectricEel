package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestKeylogWritesDailyFile(t *testing.T) {
	dir := t.TempDir()
	initKeyLog(dir)
	keylog("presence", "peek rssi=%d live=%v", -62, true)

	path := keyLogPath(dir, time.Now())
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read log: %v", err)
	}
	text := string(data)
	if !strings.Contains(text, "presence") || !strings.Contains(text, "rssi=-62") {
		t.Fatalf("log missing expected line:\n%s", text)
	}
	if !strings.Contains(text, "# ElectricEel phone-key log") {
		t.Fatal("new daily file should start with a readable header")
	}
}

func TestPruneKeyLogsDeletesOldDays(t *testing.T) {
	dir := t.TempDir()
	now := time.Date(2026, 8, 24, 12, 0, 0, 0, time.UTC)
	old := keyLogPath(dir, now.AddDate(0, 0, -8))
	keep := keyLogPath(dir, now.AddDate(0, 0, -2))
	if err := os.WriteFile(old, []byte("old\n"), 0644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(keep, []byte("keep\n"), 0644); err != nil {
		t.Fatal(err)
	}

	keyLogMu.Lock()
	pruneKeyLogsLocked(dir, 7, now)
	keyLogMu.Unlock()

	if _, err := os.Stat(old); !os.IsNotExist(err) {
		t.Fatalf("expected %s to be pruned, err=%v", filepath.Base(old), err)
	}
	if _, err := os.Stat(keep); err != nil {
		t.Fatalf("expected %s to be kept: %v", filepath.Base(keep), err)
	}
}

func TestDefaultKeyLogDirPrefersEnv(t *testing.T) {
	t.Setenv(keyLogEnvDir, "/tmp/eel-logs")
	if got := defaultKeyLogDir(); got != "/tmp/eel-logs" {
		t.Fatalf("defaultKeyLogDir = %q, want env override", got)
	}
}
