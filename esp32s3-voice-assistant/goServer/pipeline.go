package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"math"
	"net/http"
	"strings"
	"sync"
	"time"
)

const systemPrompt = "你是一个运行在ESP32小音箱里的语音助手。回答要短：一句话、40个字以内。不要使用Markdown、列表或表情符号；不确定就直接说不知道。"

type ChatMessage struct {
	Role    string `json:"role"`
	Content string `json:"content"`
}

type deepSeekRequest struct {
	Model       string        `json:"model"`
	Messages    []ChatMessage `json:"messages"`
	Stream      bool          `json:"stream"`
	MaxTokens   int           `json:"max_tokens"`
	Temperature float64       `json:"temperature"`
}

type deepSeekChunk struct {
	Choices []struct {
		Delta struct {
			Content string `json:"content"`
		} `json:"delta"`
	} `json:"choices"`
}

type synthesizedAudio struct {
	text string
	pcm  []byte
}

func (s *session) respond(userText string) error {
	s.history = append(s.history, ChatMessage{Role: "user", Content: userText})
	if len(s.history) > 12 {
		s.history = append([]ChatMessage(nil), s.history[len(s.history)-12:]...)
	}

	started := time.Now()
	textCh := make(chan string, 8)
	audioCh := make(chan synthesizedAudio, 4)
	var workers sync.WaitGroup
	var playing bool
	var playErr error

	workers.Add(1)
	go func() {
		defer workers.Done()
		defer close(audioCh)
		for sentence := range textCh {
			audio, err := s.synthesize(sentence)
			if err != nil {
				log.Printf("TTS sentence failed: %v", err)
				continue
			}
			audioCh <- synthesizedAudio{text: sentence, pcm: audio}
		}
	}()

	playDone := make(chan struct{})
	go func() {
		defer close(playDone)
		sender := &pcmSender{conn: s.conn}
		for item := range audioCh {
			if !playing {
				playing = true
				log.Printf("First audio in %dms: %s", time.Since(started).Milliseconds(), item.text)
				if err := s.conn.writeJSON(map[string]string{"type": "tts", "state": "start"}); err != nil {
					playErr = err
					return
				}
			}
			if err := sender.send(item.pcm); err != nil {
				playErr = err
				return
			}
		}
	}()

	var reply strings.Builder
	var textBuf string
	ctx, cancel := context.WithTimeout(context.Background(), 45*time.Second)
	defer cancel()
	err := s.streamChat(ctx, func(delta string) {
		reply.WriteString(delta)
		textBuf += delta
		for {
			sentence, rest := cutSentence(textBuf)
			if sentence == "" {
				break
			}
			textCh <- sentence
			textBuf = rest
		}
	})
	if err != nil {
		log.Printf("LLM stream failed: %v", err)
		if reply.Len() == 0 {
			fallback := "抱歉，网络好像不太好。"
			reply.WriteString(fallback)
			textBuf = fallback
		}
	}
	if tail := strings.TrimSpace(textBuf); tail != "" {
		textCh <- tail
	}
	close(textCh)
	workers.Wait()
	<-playDone

	answer := strings.TrimSpace(reply.String())
	if answer != "" {
		s.history = append(s.history, ChatMessage{Role: "assistant", Content: answer})
		_ = s.conn.writeJSON(map[string]string{"type": "llm", "text": answer})
	}
	_ = s.conn.writeJSON(map[string]string{"type": "tts", "state": "stop"})
	return playErr
}

func (s *session) streamChat(ctx context.Context, onDelta func(string)) error {
	if s.llm.Key == "" {
		onDelta("还没配置DeepSeek的API Key。")
		return nil
	}
	messages := make([]ChatMessage, 0, len(s.history)+1)
	messages = append(messages, ChatMessage{Role: "system", Content: systemPrompt})
	messages = append(messages, s.history...)
	body, err := json.Marshal(deepSeekRequest{
		Model: s.llm.Model, Messages: messages, Stream: true, MaxTokens: 200, Temperature: 0.7,
	})
	if err != nil {
		return err
	}
	endpoint := strings.TrimRight(s.llm.Base, "/")
	if !strings.HasSuffix(endpoint, "/chat/completions") {
		endpoint += "/chat/completions"
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, endpoint, bytes.NewReader(body))
	if err != nil {
		return err
	}
	req.Header.Set("Authorization", "Bearer "+s.llm.Key)
	req.Header.Set("Content-Type", "application/json")
	resp, err := (&http.Client{Timeout: 45 * time.Second}).Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		b, _ := io.ReadAll(io.LimitReader(resp.Body, 2048))
		return fmt.Errorf("HTTP %s: %s", resp.Status, strings.TrimSpace(string(b)))
	}

	scanner := bufio.NewScanner(resp.Body)
	buf := make([]byte, 16*1024)
	scanner.Buffer(buf, 256*1024)
	for scanner.Scan() {
		line := strings.TrimSpace(scanner.Text())
		if !strings.HasPrefix(line, "data:") {
			continue
		}
		data := strings.TrimSpace(strings.TrimPrefix(line, "data:"))
		if data == "[DONE]" {
			return nil
		}
		var chunk deepSeekChunk
		if json.Unmarshal([]byte(data), &chunk) == nil && len(chunk.Choices) > 0 {
			if delta := chunk.Choices[0].Delta.Content; delta != "" {
				onDelta(delta)
			}
		}
	}
	if err := scanner.Err(); err != nil {
		return err
	}
	return nil
}

func (s *session) synthesize(text string) ([]byte, error) {
	s.g.ttsMu.Lock()
	audio := s.g.tts.Generate(text, 0, 1.0)
	s.g.ttsMu.Unlock()
	if audio == nil || len(audio.Samples) == 0 {
		return nil, errors.New("empty TTS audio")
	}
	samples := resampleLinear(audio.Samples, audio.SampleRate, sampleRate)
	var peak float32
	for _, v := range samples {
		if a := float32(math.Abs(float64(v))); a > peak {
			peak = a
		}
	}
	if peak > 0.000001 {
		scale := float32(0.35) / peak
		for i := range samples {
			samples[i] *= scale
		}
	}
	return float32ToPCM16(samples), nil
}

func cutSentence(text string) (string, string) {
	runes := []rune(text)
	const hardCut = 14
	const commaMin = 4
	for i, r := range runes {
		if strings.ContainsRune("。！？!?…\n", r) {
			j := i + 1
			for j < len(runes) && strings.ContainsRune("”’』」）)】》", runes[j]) {
				j++
			}
			return strings.TrimSpace(string(runes[:j])), string(runes[j:])
		}
		if strings.ContainsRune("，,；;、：:", r) && i+1 >= commaMin {
			return strings.TrimSpace(string(runes[:i+1])), string(runes[i+1:])
		}
	}
	if len(runes) >= hardCut {
		return strings.TrimSpace(string(runes[:hardCut])), string(runes[hardCut:])
	}
	return "", text
}
