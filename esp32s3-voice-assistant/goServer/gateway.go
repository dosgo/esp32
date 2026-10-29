package main

import (
	"encoding/json"
	"log"
	"net/http"
	"os"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/gorilla/websocket"
	sherpa "github.com/k2-fsa/sherpa-onnx-go/sherpa_onnx"
)

const (
	vadThreshold = 90
	vadOnsetMS   = 96
	vadSilenceMS = 450
	vadMinMS     = 300
	vadMaxMS     = 8000
	preRollBytes = sampleRate * 2 * 400 / 1000
)

type safeConn struct {
	ws *websocket.Conn
	mu sync.Mutex
}

func (c *safeConn) writeJSON(v any) error {
	c.mu.Lock()
	defer c.mu.Unlock()
	_ = c.ws.SetWriteDeadline(time.Now().Add(10 * time.Second))
	return c.ws.WriteJSON(v)
}

func (c *safeConn) writeBinary(b []byte) error {
	c.mu.Lock()
	defer c.mu.Unlock()
	_ = c.ws.SetWriteDeadline(time.Now().Add(10 * time.Second))
	return c.ws.WriteMessage(websocket.BinaryMessage, b)
}

type llmConfig struct {
	Key   string
	Base  string
	Model string
}

type session struct {
	conn *safeConn
	g    *gateway

	busy atomic.Bool

	stream      *sherpa.OnlineStream
	preRoll     []byte
	hotMS       int
	speechMS    int
	silenceMS   int
	utteranceMS int
	inSpeech    bool

	history []ChatMessage
	llm     llmConfig
}

func (g *gateway) handleWebSocket(w http.ResponseWriter, r *http.Request) {
	ws, err := upgrader.Upgrade(w, r, nil)
	if err != nil {
		log.Printf("websocket upgrade: %v", err)
		return
	}
	defer ws.Close()

	c := &safeConn{ws: ws}
	s := &session{
		conn: c,
		g:    g,
		llm: llmConfig{
			Key: strings.TrimSpace(os.Getenv("DEEPSEEK_API_KEY")), Base: envOrDefault("DEEPSEEK_API_URL", "https://api.deepseek.com/v1"),
			Model: envOrDefault("DEEPSEEK_MODEL", "deepseek-chat"),
		},
	}
	log.Printf("ESP32 connected: %s", r.RemoteAddr)
	defer func() {
		s.resetUtterance()
		log.Printf("ESP32 disconnected: %s", r.RemoteAddr)
	}()

	_ = ws.SetReadDeadline(time.Now().Add(45 * time.Second))
	ws.SetPongHandler(func(string) error {
		return ws.SetReadDeadline(time.Now().Add(45 * time.Second))
	})

	for {
		msgType, data, err := ws.ReadMessage()
		if err != nil {
			return
		}
		_ = ws.SetReadDeadline(time.Now().Add(45 * time.Second))
		switch msgType {
		case websocket.TextMessage:
			s.handleText(data)
		case websocket.BinaryMessage:
			s.handleAudio(data)
		}
	}
}

func (s *session) handleText(data []byte) {
	var msg struct {
		Type   string `json:"type"`
		Config struct {
			LLM struct {
				Key   string `json:"key"`
				Base  string `json:"base"`
				Model string `json:"model"`
			} `json:"llm"`
		} `json:"config"`
	}
	if json.Unmarshal(data, &msg) != nil {
		return
	}
	switch msg.Type {
	case "hb", "ping":
		_ = s.conn.writeJSON(map[string]string{"type": "pong"})
	case "abort":
		s.resetUtterance()
	case "hello":
		if v := strings.TrimSpace(msg.Config.LLM.Key); v != "" {
			s.llm.Key = v
		}
		if v := strings.TrimSpace(msg.Config.LLM.Base); v != "" {
			s.llm.Base = strings.TrimRight(v, "/")
		}
		if v := strings.TrimSpace(msg.Config.LLM.Model); v != "" {
			s.llm.Model = v
		}
		_ = s.conn.writeJSON(map[string]any{
			"type": "hello", "transport": "websocket",
			"audio_params": map[string]any{"format": "pcm", "sample_rate": sampleRate, "frame_ms": frameMS},
		})
	}
}

func (s *session) handleAudio(data []byte) {
	if s.busy.Load() || len(data) < 2 {
		return
	}
	pcm := append([]byte(nil), data[:len(data)/2*2]...)
	stripFrameImpulse(pcm)
	frames := len(pcm) / frameBytes
	if frames < 1 {
		frames = 1
	}
	ms := frames * frameMS
	energy := pcmRMS(pcm)

	if !s.inSpeech {
		s.preRoll = append(s.preRoll, pcm...)
		if len(s.preRoll) > preRollBytes {
			s.preRoll = append([]byte(nil), s.preRoll[len(s.preRoll)-preRollBytes:]...)
		}
		if energy > vadThreshold {
			s.hotMS += ms
		} else {
			s.hotMS = 0
		}
		if s.hotMS < vadOnsetMS {
			return
		}

		s.inSpeech = true
		s.speechMS = s.hotMS
		s.utteranceMS = len(s.preRoll) * 1000 / (sampleRate * 2)
		s.stream = sherpa.NewOnlineStream(s.g.asr)
		s.feedASR(s.preRoll)
		return
	}

	s.feedASR(pcm)
	s.utteranceMS += ms
	if energy > vadThreshold {
		s.speechMS += ms
		s.silenceMS = 0
	} else {
		s.silenceMS += ms
	}

	if s.silenceMS < vadSilenceMS && s.utteranceMS < vadMaxMS {
		return
	}
	if s.speechMS < vadMinMS {
		log.Printf("VAD ignored short fragment: %dms", s.speechMS)
		s.resetUtterance()
		return
	}

	text := s.finishASR()
	s.resetUtterance()
	if text == "" {
		return
	}
	log.Printf("ASR final: %s", text)
	_ = s.conn.writeJSON(map[string]string{"type": "stt", "text": text})
	if !s.busy.CompareAndSwap(false, true) {
		return
	}
	go func() {
		defer s.busy.Store(false)
		if err := s.respond(text); err != nil {
			log.Printf("response pipeline: %v", err)
		}
	}()
}

func (s *session) feedASR(pcm []byte) {
	if s.stream == nil || len(pcm) < 2 {
		return
	}
	s.g.asrMu.Lock()
	defer s.g.asrMu.Unlock()
	s.stream.AcceptWaveform(sampleRate, pcm16ToFloat32(pcm))
	for s.g.asr.IsReady(s.stream) {
		s.g.asr.Decode(s.stream)
	}
}

func (s *session) finishASR() string {
	if s.stream == nil {
		return ""
	}
	s.g.asrMu.Lock()
	defer s.g.asrMu.Unlock()
	s.stream.InputFinished()
	for s.g.asr.IsReady(s.stream) {
		s.g.asr.Decode(s.stream)
	}
	return strings.TrimSpace(s.g.asr.GetResult(s.stream).Text)
}

func (s *session) resetUtterance() {
	if s.stream != nil {
		s.g.asrMu.Lock()
		sherpa.DeleteOnlineStream(s.stream)
		s.g.asrMu.Unlock()
	}
	s.stream = nil
	s.preRoll = nil
	s.hotMS, s.speechMS, s.silenceMS, s.utteranceMS = 0, 0, 0, 0
	s.inSpeech = false
}
