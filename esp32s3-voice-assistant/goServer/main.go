package main

import (
	"log"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"

	"github.com/gorilla/websocket"
	sherpa "github.com/k2-fsa/sherpa-onnx-go/sherpa_onnx"
)

const (
	sampleRate = 16000
	frameMS    = 32
	frameBytes = sampleRate * 2 * frameMS / 1000
)

var upgrader = websocket.Upgrader{CheckOrigin: func(r *http.Request) bool { return true }}

type gateway struct {
	asr   *sherpa.OnlineRecognizer
	tts   *sherpa.OfflineTts
	asrMu sync.Mutex
	ttsMu sync.Mutex
}

func main() {
	asrDir := envOrDefault("ASR_MODEL_DIR", "models/sherpa-onnx-streaming-zipformer-zh-14M-2023-02-23")
	asr := sherpa.NewOnlineRecognizer(&sherpa.OnlineRecognizerConfig{
		FeatConfig: sherpa.FeatureConfig{SampleRate: sampleRate, FeatureDim: 80},
		ModelConfig: sherpa.OnlineModelConfig{
			Transducer: sherpa.OnlineTransducerModelConfig{
				Encoder: filepath.Join(asrDir, "encoder-epoch-99-avg-1.int8.onnx"),
				Decoder: filepath.Join(asrDir, "decoder-epoch-99-avg-1.int8.onnx"),
				Joiner:  filepath.Join(asrDir, "joiner-epoch-99-avg-1.int8.onnx"),
			},
			Tokens: filepath.Join(asrDir, "tokens.txt"), NumThreads: 4, Provider: "cpu",
		},
		DecodingMethod: "greedy_search",
	})
	if asr == nil {
		log.Fatal("failed to initialize ASR; check ASR_MODEL_DIR")
	}
	defer sherpa.DeleteOnlineRecognizer(asr)
	log.Println("Streaming ASR initialized")

	ttsDir := envOrDefault("TTS_MODEL_DIR", "models/vits-icefall-zh-aishell3")
	tts := sherpa.NewOfflineTts(&sherpa.OfflineTtsConfig{
		Model: sherpa.OfflineTtsModelConfig{
			Vits: sherpa.OfflineTtsVitsModelConfig{
				Model: filepath.Join(ttsDir, "model.onnx"), Tokens: filepath.Join(ttsDir, "tokens.txt"),
				Lexicon: filepath.Join(ttsDir, "lexicon.txt"),
			},
			NumThreads: 2, Provider: "cpu",
		},
		RuleFars: filepath.Join(ttsDir, "rule.far"),
	})
	if tts == nil {
		log.Fatal("failed to initialize TTS; check TTS_MODEL_DIR")
	}
	defer sherpa.DeleteOfflineTts(tts)
	log.Printf("TTS initialized: %d Hz, %d speakers", tts.SampleRate(), tts.NumSpeakers())

	g := &gateway{asr: asr, tts: tts}
	http.HandleFunc("/ws", g.handleWebSocket)
	http.HandleFunc("/health", func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"status":"ok"}`))
	})
	addr := ":" + envOrDefault("GATEWAY_PORT", "8765")
	log.Printf("Voice gateway started on %s (ws path /ws)", addr)
	log.Fatal(http.ListenAndServe(addr, nil))
}

func envOrDefault(name, fallback string) string {
	if value := strings.TrimSpace(os.Getenv(name)); value != "" {
		return value
	}
	return fallback
}
