package main

import (
	"encoding/binary"
	"math"
	"time"
)

func pcm16ToFloat32(b []byte) []float32 {
	n := len(b) / 2
	out := make([]float32, n)
	for i := range out {
		out[i] = float32(int16(binary.LittleEndian.Uint16(b[i*2:]))) / 32768
	}
	return out
}

func float32ToPCM16(samples []float32) []byte {
	out := make([]byte, len(samples)*2)
	for i, sample := range samples {
		if sample > 1 {
			sample = 1
		} else if sample < -1 {
			sample = -1
		}
		binary.LittleEndian.PutUint16(out[i*2:], uint16(int16(sample*32767)))
	}
	return out
}

func pcmRMS(pcm []byte) int {
	n := len(pcm) / 2
	if n == 0 {
		return 0
	}
	var sum float64
	for i := 0; i < n; i++ {
		v := float64(int16(binary.LittleEndian.Uint16(pcm[i*2:])))
		sum += v * v
	}
	return int(math.Sqrt(sum / float64(n)))
}

func stripFrameImpulse(pcm []byte) {
	const blockSamples = 512
	for sample := 0; sample+1 < len(pcm)/2; sample += blockSamples {
		copy(pcm[sample*2:sample*2+2], pcm[(sample+1)*2:(sample+1)*2+2])
	}
}

func resampleLinear(in []float32, srcRate, dstRate int) []float32 {
	if len(in) == 0 || srcRate <= 0 || dstRate <= 0 {
		return nil
	}
	if srcRate == dstRate {
		return append([]float32(nil), in...)
	}
	outN := int(math.Round(float64(len(in)) * float64(dstRate) / float64(srcRate)))
	out := make([]float32, outN)
	ratio := float64(srcRate) / float64(dstRate)
	for i := range out {
		pos := float64(i) * ratio
		left := int(pos)
		if left >= len(in)-1 {
			out[i] = in[len(in)-1]
			continue
		}
		frac := float32(pos - float64(left))
		out[i] = in[left]*(1-frac) + in[left+1]*frac
	}
	return out
}

type pcmSender struct{ conn *safeConn }

func (s *pcmSender) send(pcm []byte) error {
	next := time.Now()
	frames := 0
	for len(pcm) > 0 {
		chunk := make([]byte, frameBytes)
		n := copy(chunk, pcm)
		pcm = pcm[n:]
		if frames >= 8 {
			next = next.Add(frameMS * time.Millisecond)
			if delay := time.Until(next); delay > 0 {
				time.Sleep(delay)
			} else if delay < -150*time.Millisecond {
				next = time.Now()
			}
		}
		if err := s.conn.writeBinary(chunk); err != nil {
			return err
		}
		frames++
	}
	return nil
}
