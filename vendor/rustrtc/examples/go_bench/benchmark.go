package main

import (
	"fmt"
	"log"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/pion/webrtc/v3"
)

type BenchResult struct {
	Mode      string
	Duration  time.Duration
	DcLatency float64
	Bytes     uint64
	Msgs      uint64
	CpuUsage  float64
	MemoryRss uint64
}

func (b *BenchResult) Print() {
	fmt.Println("\n------------------------------------------------")
	fmt.Printf("Benchmark Results (%s)\n", b.Mode)
	fmt.Println("------------------------------------------------")
	fmt.Printf("Total Duration:      %.2fs\n", b.Duration.Seconds())
	fmt.Printf("Setup Latency:       %.2f ms (avg)\n", b.DcLatency)
	fmt.Printf("Total Data:          %.2f MB\n", float64(b.Bytes)/1024.0/1024.0)
	fmt.Printf("Total Messages:      %d\n", b.Msgs)
	fmt.Printf("Throughput:          %.2f MB/s\n", b.Throughput())
	fmt.Printf("Message Rate:        %.2f msg/s\n", b.MsgRate())
	fmt.Printf("Avg CPU Usage:       %.2f%%\n", b.CpuUsage)
	fmt.Printf("Peak Memory RSS:     %d MB\n", b.MemoryRss)
	fmt.Println("------------------------------------------------")
}

func (b *BenchResult) Throughput() float64 {
	if b.Duration.Seconds() > 0 {
		return (float64(b.Bytes) / 1024.0 / 1024.0) / b.Duration.Seconds()
	}
	return 0
}

func (b *BenchResult) MsgRate() float64 {
	if b.Duration.Seconds() > 0 {
		return float64(b.Msgs) / b.Duration.Seconds()
	}
	return 0
}

func main() {
	count := 10
	if len(os.Args) > 1 {
		if c, err := strconv.Atoi(os.Args[1]); err == nil {
			count = c
		}
	}

	runBenchmark("pion", count).Print()
}

func runBenchmark(mode string, count int) *BenchResult {
	fmt.Printf("Starting benchmark: mode=%s, count=%d\n", mode, count)

	peakRss, avgCpu, cpuSamples, running := startResourceMonitor()

	start := time.Now()
	dcLatency, totalBytes, totalMsgs := runPion(count)
	duration := time.Since(start)

	atomic.StoreInt32(running, 0)

	samples := atomic.LoadUint64(cpuSamples)
	avgCpuVal := 0.0
	if samples > 0 {
		avgCpuVal = float64(atomic.LoadUint64(avgCpu)) / float64(samples) / 100.0 // Fixed point adjustment
	}

	return &BenchResult{
		Mode:      mode,
		Duration:  duration,
		DcLatency: dcLatency,
		Bytes:     totalBytes,
		Msgs:      totalMsgs,
		CpuUsage:  avgCpuVal,
		MemoryRss: atomic.LoadUint64(peakRss),
	}
}

func startResourceMonitor() (*uint64, *uint64, *uint64, *int32) {
	pid := os.Getpid()
	peakRss := new(uint64)
	avgCpu := new(uint64)
	cpuSamples := new(uint64)
	running := new(int32)
	*running = 1

	go func() {
		for atomic.LoadInt32(running) == 1 {
			// exec.Command is heavy, so we run it less frequently
			cmd := exec.Command("ps", "-o", "rss,%cpu", "-p", strconv.Itoa(pid))
			out, err := cmd.Output()
			if err == nil {
				lines := strings.Split(string(out), "\n")
				if len(lines) >= 2 {
					fields := strings.Fields(lines[1])
					if len(fields) >= 2 {
						if rss, err := strconv.ParseUint(fields[0], 10, 64); err == nil {
							// RSS is in KB
							currentRss := rss / 1024 // MB
							for {
								oldPeak := atomic.LoadUint64(peakRss)
								if currentRss <= oldPeak {
									break
								}
								if atomic.CompareAndSwapUint64(peakRss, oldPeak, currentRss) {
									break
								}
							}
						}
						if cpu, err := strconv.ParseFloat(fields[1], 64); err == nil {
							currentCpu := uint64(cpu * 100.0)
							atomic.AddUint64(avgCpu, currentCpu)
							atomic.AddUint64(cpuSamples, 1)
						}
					}
				}
			}
			time.Sleep(2000 * time.Millisecond)
		}
	}()

	return peakRss, avgCpu, cpuSamples, running
}

func runPion(count int) (float64, uint64, uint64) {
	var wg sync.WaitGroup
	var totalBytes uint64
	var totalMsgs uint64
	var totalDcLatency uint64

	// Limit concurrency to avoid file descriptor limits or excessive resource usage if count is high
	// The rust version spawns all at once, but let's be safe or just match it.
	// Rust: handles.push(tokio::spawn(...)) for _ in 0..count
	// So it spawns all. We will do the same.

	for i := 0; i < count; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()

			// Setup MediaEngine
			m := &webrtc.MediaEngine{}
			if err := m.RegisterDefaultCodecs(); err != nil {
				log.Println(err)
				return
			}

			api := webrtc.NewAPI(webrtc.WithMediaEngine(m))

			config := webrtc.Configuration{}

			pc1, err := api.NewPeerConnection(config)
			if err != nil {
				log.Println(err)
				return
			}
			pc2, err := api.NewPeerConnection(config)
			if err != nil {
				log.Println(err)
				return
			}

			// Add audio track to pc1
			// Create a dummy track
			track, err := webrtc.NewTrackLocalStaticSample(webrtc.RTPCodecCapability{MimeType: webrtc.MimeTypeOpus}, "audio", "pion")
			if err != nil {
				log.Println(err)
				return
			}
			if _, err = pc1.AddTrack(track); err != nil {
				log.Println(err)
				return
			}

			dc1, err := pc1.CreateDataChannel("bench", nil)
			if err != nil {
				log.Println(err)
				return
			}

			done := make(chan struct{})

			pc2.OnDataChannel(func(dc2 *webrtc.DataChannel) {
				dc2.OnMessage(func(msg webrtc.DataChannelMessage) {
					atomic.AddUint64(&totalBytes, uint64(len(msg.Data)))
					atomic.AddUint64(&totalMsgs, 1)
				})
				dc2.OnClose(func() {
					close(done)
				})
			})

			// Exchange SDP
			offer, err := pc1.CreateOffer(nil)
			if err != nil {
				log.Println(err)
				return
			}

			// Create channel that is blocked until ICE Gathering is complete
			gatherComplete := webrtc.GatheringCompletePromise(pc1)
			if err = pc1.SetLocalDescription(offer); err != nil {
				log.Println(err)
				return
			}
			<-gatherComplete

			if err = pc2.SetRemoteDescription(*pc1.LocalDescription()); err != nil {
				log.Println(err)
				return
			}

			answer, err := pc2.CreateAnswer(nil)
			if err != nil {
				log.Println(err)
				return
			}

			gatherComplete = webrtc.GatheringCompletePromise(pc2)
			if err = pc2.SetLocalDescription(answer); err != nil {
				log.Println(err)
				return
			}
			<-gatherComplete

			if err = pc1.SetRemoteDescription(*pc2.LocalDescription()); err != nil {
				log.Println(err)
				return
			}

			// Wait for connection
			connected := make(chan struct{})
			pc1.OnConnectionStateChange(func(s webrtc.PeerConnectionState) {
				if s == webrtc.PeerConnectionStateConnected {
					select {
					case <-connected:
					default:
						close(connected)
					}
				}
			})

			select {
			case <-connected:
			case <-time.After(10 * time.Second):
				// log.Println("Timeout waiting for connection")
				pc1.Close()
				pc2.Close()
				return
			}

			// Wait for DC open
			dcWaitStart := time.Now()
			dcOpen := make(chan struct{})
			dc1.OnOpen(func() {
				close(dcOpen)
			})

			select {
			case <-dcOpen:
			case <-time.After(5 * time.Second):
				// log.Println("Timeout waiting for data channel open")
				pc1.Close()
				pc2.Close()
				return
			}
			dcLatency := time.Since(dcWaitStart).Milliseconds()
			atomic.AddUint64(&totalDcLatency, uint64(dcLatency))

			// Send data
			data := make([]byte, 1024)
			startSend := time.Now()
			duration := 10 * time.Second

			// Configure backpressure
			threshold := uint64(100 * 1024)
			dc1.SetBufferedAmountLowThreshold(threshold)

			// Channel to signal when we can send again
			canSend := make(chan struct{}, 1)
			// Initially we can send
			canSend <- struct{}{}

			dc1.OnBufferedAmountLow(func() {
				select {
				case canSend <- struct{}{}:
				default:
				}
			})

			for time.Since(startSend) < duration {
				if dc1.BufferedAmount() > threshold {
					<-canSend
				}
				if err := dc1.Send(data); err != nil {
					log.Printf("Send error: %v\n", err)
					break
				}
			}

			pc1.Close()
			pc2.Close()

			select {
			case <-done:
			case <-time.After(5 * time.Second):
			}
		}()
	}

	wg.Wait()

	avgDcLatency := 0.0
	if count > 0 {
		avgDcLatency = float64(atomic.LoadUint64(&totalDcLatency)) / float64(count)
	}

	return avgDcLatency, atomic.LoadUint64(&totalBytes), atomic.LoadUint64(&totalMsgs)
}
