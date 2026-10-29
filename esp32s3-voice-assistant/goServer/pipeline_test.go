package main

import "testing"

func TestCutSentence(t *testing.T) {
	tests := []struct {
		in, want, rest string
	}{
		{"你好，我是小特。后半句", "你好，我是小特。", "后半句"},
		{"短，继续", "", "短，继续"},
		{"这是没有任何标点但已经足够长的一句话", "这是没有任何标点但已经足够长", "的一句话"},
	}
	for _, tt := range tests {
		got, rest := cutSentence(tt.in)
		if got != tt.want || rest != tt.rest {
			t.Fatalf("cutSentence(%q) = (%q, %q), want (%q, %q)", tt.in, got, rest, tt.want, tt.rest)
		}
	}
}

func TestResampleLinear(t *testing.T) {
	in := []float32{0, 1, 0, -1}
	out := resampleLinear(in, 8000, 16000)
	if len(out) != 8 {
		t.Fatalf("resampled length = %d, want 8", len(out))
	}
	if out[0] != 0 || out[2] != 1 {
		t.Fatalf("unexpected interpolation: %v", out)
	}
}

func TestAudioFrameSize(t *testing.T) {
	if frameBytes != 1024 {
		t.Fatalf("frameBytes = %d, want 1024", frameBytes)
	}
}
