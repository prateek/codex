// scripts/e2e_hooks_cli.go
//
// End-to-end hook test for Codex CLI using a local stub Responses API server.
//
// Usage (from the codex repo root):
//
//	go run ./scripts/e2e_hooks_cli.go --profile debug --build
//	go run ./scripts/e2e_hooks_cli.go --profile release --build
//
// What it does:
// - Builds (optional) and runs `codex exec ...` against a stub HTTP server.
// - Writes a temporary CODEX_HOME/config.toml that points model requests to the stub server.
// - Configures `[hooks]` to run a tiny script that records one JSON line per hook invocation.
// - Asserts the expected hook events fire in order: turn_started → exec_command_begin → exec_command_end → turn_complete.
package main

import (
	"bytes"
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"
)

type profile string

const (
	profileDebug   profile = "debug"
	profileRelease profile = "release"
)

type stubServer struct {
	mu           sync.Mutex
	sseQueue     []string
	responseReqs []map[string]any
}

func (s *stubServer) popSSE() (string, bool) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if len(s.sseQueue) == 0 {
		return "", false
	}
	body := s.sseQueue[0]
	s.sseQueue = s.sseQueue[1:]
	return body, true
}

func (s *stubServer) recordResponseRequest(body map[string]any) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.responseReqs = append(s.responseReqs, body)
}

func (s *stubServer) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	switch {
	case r.Method == http.MethodGet && strings.HasSuffix(r.URL.Path, "/models"):
		w.Header().Set("content-type", "application/json")
		_, _ = io.WriteString(w, `{"models":[]}`)
		return
	case r.Method == http.MethodPost && strings.HasSuffix(r.URL.Path, "/responses"):
		raw, _ := io.ReadAll(r.Body)
		_ = r.Body.Close()
		var parsed map[string]any
		_ = json.Unmarshal(raw, &parsed)
		if len(parsed) != 0 {
			s.recordResponseRequest(parsed)
		}

		body, ok := s.popSSE()
		if !ok {
			http.Error(w, "no queued SSE responses", http.StatusInternalServerError)
			return
		}
		w.Header().Set("content-type", "text/event-stream")
		_, _ = io.WriteString(w, body)
		if f, ok := w.(http.Flusher); ok {
			f.Flush()
		}
		return
	case r.Method == http.MethodPost && strings.HasSuffix(r.URL.Path, "/responses/compact"):
		// Not expected for this test, but return a harmless response if core requests it.
		w.Header().Set("content-type", "application/json")
		_, _ = io.WriteString(w, `{"summary":"ok"}`)
		return
	default:
		http.NotFound(w, r)
	}
}

func sse(events ...map[string]any) string {
	var b strings.Builder
	for _, ev := range events {
		kind, _ := ev["type"].(string)
		_, _ = fmt.Fprintf(&b, "event: %s\n", kind)
		if len(ev) == 1 {
			b.WriteByte('\n')
			continue
		}
		payload, _ := json.Marshal(ev)
		_, _ = fmt.Fprintf(&b, "data: %s\n\n", payload)
	}
	return b.String()
}

func evResponseCreated(id string) map[string]any {
	return map[string]any{
		"type": "response.created",
		"response": map[string]any{
			"id": id,
		},
	}
}

func evCompleted(id string) map[string]any {
	return map[string]any{
		"type": "response.completed",
		"response": map[string]any{
			"id": id,
			"usage": map[string]any{
				"input_tokens":          0,
				"input_tokens_details":  nil,
				"output_tokens":         0,
				"output_tokens_details": nil,
				"total_tokens":          0,
			},
		},
	}
}

func evFunctionCallDone(callID, name, arguments string) map[string]any {
	return map[string]any{
		"type": "response.output_item.done",
		"item": map[string]any{
			"type":      "function_call",
			"call_id":   callID,
			"name":      name,
			"arguments": arguments,
		},
	}
}

func evAssistantMessageDone(id, text string) map[string]any {
	return map[string]any{
		"type": "response.output_item.done",
		"item": map[string]any{
			"type": "message",
			"role": "assistant",
			"id":   id,
			"content": []map[string]any{
				{"type": "output_text", "text": text},
			},
		},
	}
}

func writeExecutable(path, content string) error {
	return os.WriteFile(path, []byte(content), 0o755)
}

type hookCall struct {
	Seq          int
	SeqStr       string
	Expected     string
	Event        string
	SubmissionID string
	Path         string
}

type hookCallJSON struct {
	Seq          string `json:"seq"`
	Expected     string `json:"expected"`
	Event        string `json:"event"`
	SubmissionID string `json:"submission_id"`
}

func readHookCallsOnce(dir string) ([]hookCall, error) {
	entries, err := os.ReadDir(dir)
	if err != nil {
		return nil, err
	}
	var calls []hookCall
	for _, ent := range entries {
		if ent.IsDir() {
			continue
		}
		base := ent.Name()
		seqStr := strings.TrimSuffix(base, filepath.Ext(base))
		seq, err := strconv.Atoi(seqStr)
		if err != nil {
			continue
		}
		path := filepath.Join(dir, base)
		b, err := os.ReadFile(path)
		if err != nil {
			return nil, err
		}
		var parsed hookCallJSON
		if err := json.Unmarshal(b, &parsed); err != nil {
			return nil, fmt.Errorf("parse hook record %s: %w", path, err)
		}
		calls = append(calls, hookCall{
			Seq:          seq,
			SeqStr:       parsed.Seq,
			Expected:     parsed.Expected,
			Event:        parsed.Event,
			SubmissionID: parsed.SubmissionID,
			Path:         path,
		})
	}
	sort.Slice(calls, func(i, j int) bool { return calls[i].Seq < calls[j].Seq })
	return calls, nil
}

func listHookCalls(dir string, wantAtLeast int, timeout time.Duration) ([]hookCall, error) {
	deadline := time.Now().Add(timeout)
	quietFor := 200 * time.Millisecond
	var lastCount int
	var stableSince time.Time
	for {
		calls, err := readHookCallsOnce(dir)
		if err != nil {
			return nil, err
		}
		if len(calls) >= wantAtLeast {
			if len(calls) == lastCount {
				if stableSince.IsZero() {
					stableSince = time.Now()
				} else if time.Since(stableSince) >= quietFor {
					return calls, nil
				}
			} else {
				stableSince = time.Time{}
			}
		}
		lastCount = len(calls)
		if time.Now().After(deadline) {
			return nil, fmt.Errorf("timed out waiting for %d hook calls in %s (found %d)", wantAtLeast, dir, len(calls))
		}
		time.Sleep(25 * time.Millisecond)
	}
}

func cargoBuildCodex(codexRSDir string, prof profile) error {
	args := []string{"build", "-p", "codex-cli"}
	if prof == profileRelease {
		args = append(args, "--release")
	}
	cmd := exec.Command("cargo", args...)
	cmd.Dir = codexRSDir
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr
	return cmd.Run()
}

func main() {
	var (
		codexRepo = flag.String("codex-repo", ".", "Path to openai/codex repo root")
		prof      = flag.String("profile", string(profileDebug), "Build/profile to run: debug or release")
		build     = flag.Bool("build", false, "Build codex-cli before running")
		codexBin  = flag.String("codex-bin", "", "Path to codex binary (overrides --codex-repo/--profile)")
		keepTmp   = flag.Bool("keep-tmp", false, "Keep temporary directories on success")
		timeout   = flag.Duration("timeout", 2*time.Minute, "Overall timeout")
	)
	flag.Parse()

	profileVal := profile(*prof)
	if profileVal != profileDebug && profileVal != profileRelease {
		fmt.Fprintf(os.Stderr, "invalid --profile %q (expected debug or release)\n", *prof)
		os.Exit(2)
	}

	codexRSDir := filepath.Join(*codexRepo, "codex-rs")
	if abs, err := filepath.Abs(codexRSDir); err == nil {
		codexRSDir = abs
	}
	binPath := *codexBin
	if binPath == "" {
		binPath = filepath.Join(codexRSDir, "target", string(profileVal), "codex")
	}
	if abs, err := filepath.Abs(binPath); err == nil {
		binPath = abs
	}

	if *build {
		if err := cargoBuildCodex(codexRSDir, profileVal); err != nil {
			fmt.Fprintf(os.Stderr, "cargo build failed: %v\n", err)
			os.Exit(1)
		}
	}
	if _, err := os.Stat(binPath); err != nil {
		// Try building automatically if missing.
		if err := cargoBuildCodex(codexRSDir, profileVal); err != nil {
			fmt.Fprintf(os.Stderr, "codex binary missing at %s and build failed: %v\n", binPath, err)
			os.Exit(1)
		}
	}

	ctx, cancel := context.WithTimeout(context.Background(), *timeout)
	defer cancel()

	tmp, err := os.MkdirTemp("", "codex-hooks-e2e-*")
	if err != nil {
		fmt.Fprintf(os.Stderr, "mkdtemp: %v\n", err)
		os.Exit(1)
	}
	if !*keepTmp {
		defer os.RemoveAll(tmp)
	} else {
		fmt.Fprintf(os.Stderr, "keeping tmp dir: %s\n", tmp)
	}

	codexHome := filepath.Join(tmp, "codex_home")
	workspace := filepath.Join(tmp, "workspace")
	hookDir := filepath.Join(tmp, "hooks")
	callsDir := filepath.Join(hookDir, "calls")
	if err := os.MkdirAll(codexHome, 0o755); err != nil {
		fmt.Fprintf(os.Stderr, "mkdir codex_home: %v\n", err)
		os.Exit(1)
	}
	if err := os.MkdirAll(workspace, 0o755); err != nil {
		fmt.Fprintf(os.Stderr, "mkdir workspace: %v\n", err)
		os.Exit(1)
	}
	if err := os.MkdirAll(callsDir, 0o755); err != nil {
		fmt.Fprintf(os.Stderr, "mkdir calls: %v\n", err)
		os.Exit(1)
	}

	hookScript := filepath.Join(hookDir, "hook.sh")
	if err := writeExecutable(hookScript, fmt.Sprintf(`#!/bin/sh
set -eu
out_dir=%q
mkdir -p "$out_dir"
expected="${1:-unset}"
seq="${CODEX_HOOK_SEQ:-unset}"
event="${CODEX_HOOK_EVENT:-unset}"
submission_id="${CODEX_HOOK_SUBMISSION_ID:-unset}"
tmp="$out_dir/$seq.json.tmp.$$"
printf '{"seq":"%%s","expected":"%%s","event":"%%s","submission_id":"%%s"}\n' "$seq" "$expected" "$event" "$submission_id" > "$tmp"
mv "$tmp" "$out_dir/$seq.json"
`, callsDir)); err != nil {
		fmt.Fprintf(os.Stderr, "write hook script: %v\n", err)
		os.Exit(1)
	}

	// Stub server (Responses API)
	targetFile := filepath.Join(workspace, "shell_ran.txt")
	toolArgs, _ := json.Marshal(map[string]any{
		"command":    fmt.Sprintf("echo hook-ok > %s", shellQuote(targetFile)),
		"timeout_ms": 1000,
	})

	stub := &stubServer{
		sseQueue: []string{
			sse(
				evResponseCreated("resp-1"),
				evFunctionCallDone("call-1", "shell_command", string(toolArgs)),
				evCompleted("resp-1"),
			),
			sse(
				evResponseCreated("resp-2"),
				evAssistantMessageDone("msg-1", "done"),
				evCompleted("resp-2"),
			),
		},
	}

	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		fmt.Fprintf(os.Stderr, "listen: %v\n", err)
		os.Exit(1)
	}
	defer ln.Close()
	addr := ln.Addr().String()
	baseURL := fmt.Sprintf("http://%s/v1", addr)
	srv := &http.Server{Handler: stub}
	go func() {
		_ = srv.Serve(ln)
	}()
	defer func() {
		_ = srv.Shutdown(context.Background())
	}()

	// Codex config that points provider requests at our stub server and enables hooks.
	configToml := fmt.Sprintf(`
model = "gpt-5.1-codex"
model_provider = "stub"
approval_policy = "never"
sandbox_mode = "danger-full-access"
check_for_update_on_startup = false

[analytics]
enabled = false

[feedback]
enabled = false

[model_providers.stub]
name = "Stub"
base_url = %q
env_key = "OPENAI_API_KEY"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
stream_idle_timeout_ms = 30000

[hooks]
turn_started = [[%q, "turn_started"]]
exec_command_begin = [[%q, "exec_command_begin"]]
exec_command_end = [[%q, "exec_command_end"]]
turn_complete = [[%q, "turn_complete"]]
`, baseURL, hookScript, hookScript, hookScript, hookScript)

	if err := os.WriteFile(filepath.Join(codexHome, "config.toml"), []byte(configToml), 0o644); err != nil {
		fmt.Fprintf(os.Stderr, "write config.toml: %v\n", err)
		os.Exit(1)
	}

	// Run `codex exec` with the temp CODEX_HOME.
	args := []string{
		"exec",
		"--skip-git-repo-check",
		"--json",
		"--cd", workspace,
		"run one tool call",
	}
	cmd := exec.CommandContext(ctx, binPath, args...)
	cmd.Dir = workspace
	cmd.Env = filteredEnv(append(os.Environ(),
		"CODEX_HOME="+codexHome,
		"OPENAI_API_KEY=dummy",
	))
	var stdout, stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr

	if err := cmd.Run(); err != nil {
		fmt.Fprintf(os.Stderr, "codex exec failed: %v\n", err)
		if out := strings.TrimSpace(stdout.String()); out != "" {
			fmt.Fprintf(os.Stderr, "\nstdout:\n%s\n", out)
		}
		if out := strings.TrimSpace(stderr.String()); out != "" {
			fmt.Fprintf(os.Stderr, "\nstderr:\n%s\n", out)
		}
		os.Exit(1)
	}

	if _, err := os.Stat(targetFile); err != nil {
		fmt.Fprintf(os.Stderr, "expected shell command to create %s: %v\n", targetFile, err)
		os.Exit(1)
	}

	calls, err := listHookCalls(callsDir, 4, 5*time.Second)
	if err != nil {
		fmt.Fprintf(os.Stderr, "hooks did not fire as expected: %v\n", err)
		os.Exit(1)
	}
	want := []string{"turn_started", "exec_command_begin", "exec_command_end", "turn_complete"}
	got := make([]string, 0, len(calls))
	for _, c := range calls {
		if c.SeqStr != "" && c.SeqStr != strconv.Itoa(c.Seq) {
			fmt.Fprintf(os.Stderr, "hook record seq mismatch: file seq=%d json seq=%q (%s)\n", c.Seq, c.SeqStr, c.Path)
			os.Exit(1)
		}
		if c.Expected != c.Event {
			fmt.Fprintf(os.Stderr, "hook record expected/event mismatch: expected=%q event=%q seq=%d (%s)\n", c.Expected, c.Event, c.Seq, c.Path)
			os.Exit(1)
		}
		got = append(got, c.Event)
	}
	if !equalStrings(got, want) {
		fmt.Fprintf(os.Stderr, "unexpected hook sequence:\n  got:  %v\n  want: %v\n", got, want)
		os.Exit(1)
	}
	if len(calls) != len(want) {
		fmt.Fprintf(os.Stderr, "unexpected hook call count: got %d calls (%v), want %d\n", len(calls), got, len(want))
		os.Exit(1)
	}

	fmt.Println("OK")
}

func equalStrings(a, b []string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}

func filteredEnv(env []string) []string {
	// Prevent callers (including some local harnesses) from accidentally influencing
	// network / sandbox behavior.
	block := map[string]struct{}{
		"CODEX_SANDBOX_NETWORK_DISABLED": {},
		"CODEX_SANDBOX":                  {},
	}
	out := make([]string, 0, len(env))
	for _, kv := range env {
		k := kv
		if idx := strings.IndexByte(kv, '='); idx != -1 {
			k = kv[:idx]
		}
		if _, banned := block[k]; banned {
			continue
		}
		out = append(out, kv)
	}
	return out
}

func shellQuote(s string) string {
	// Minimal POSIX shell quoting: wrap in single quotes and escape embedded single quotes.
	if s == "" {
		return "''"
	}
	if !strings.ContainsRune(s, '\'') {
		return "'" + s + "'"
	}
	return "'" + strings.ReplaceAll(s, "'", `'"'"'`) + "'"
}
