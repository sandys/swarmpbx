// carrier-sim is the SIP carrier of the agentdialer tests, built on diago
// (github.com/emiago/diago). It behaves like Twilio Elastic SIP Trunking
// termination, a carrier that the dialer supports, so the tests that run on
// every change check the behavior that Twilio needs:
//
//   - the Request-URI user must be E.164 ("+" and digits), else 400;
//   - with SIM_AUTH_USER set, every new INVITE gets a 407 digest challenge
//     (realm SIM_REALM, default sip.twilio.com); bad credentials get 403;
//   - with SIM_CALLER_IDS set, the From user must be one of them, else 403
//     (Twilio: a number of the account or a verified caller ID);
//   - SIM_SYMMETRIC_RTP=1 sends RTP to the address that it comes from
//     (Twilio's "Symmetric RTP"); 0 sends it to the address in the SDP.
//
// Each number has a behavior (SIM_NUMBERS, for example
// "+15550000486=busy,+15550000408=noanswer"); other numbers get
// SIM_DEFAULT (default echo):
//
//	echo        answer, send each RTP payload back, BYE after SIM_HOLD_MS
//	tone        answer, send a 1 kHz tone, BYE after SIM_HOLD_MS
//	busy        486    decline  603    unavailable  480    fail  503
//	noanswer    180, never answer (the caller cancels)
//
// GET /calls on SIM_HTTP_ADDR returns what the carrier saw, per call:
// the INVITEs, the auth result, the status, the RTP counts, and how many
// received packets carried a tone (loud). POST /reset clears it.
package main

import (
	"context"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"math"
	"net"
	"net/http"
	"os"
	"os/signal"
	"sort"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/emiago/diago"
	"github.com/emiago/diago/audio"
	"github.com/emiago/diago/media"
	"github.com/emiago/sipgo"
	"github.com/emiago/sipgo/sip"
	"github.com/icholy/digest"
)

// A received packet whose peak passes this carries a tone, not silence.
const loudPeak = 2000

// Call is what the carrier saw of one call (one Call-ID).
type Call struct {
	CallID     string     `json:"call_id"`
	From       string     `json:"from"`
	To         string     `json:"to"`
	Behavior   string     `json:"behavior"`
	Invites    int        `json:"invites"`
	Auth       string     `json:"auth"`
	Status     string     `json:"status"`
	Reason     string     `json:"reason,omitempty"`
	SDPAddr    string     `json:"sdp_addr,omitempty"`
	RTPIn      int64      `json:"rtp_in"`
	LoudIn     int64      `json:"loud_in"`
	RTPOut     int64      `json:"rtp_out"`
	StartedAt  time.Time  `json:"started_at"`
	AnsweredAt *time.Time `json:"answered_at,omitempty"`
	EndedAt    *time.Time `json:"ended_at,omitempty"`
	EndedBy    string     `json:"ended_by,omitempty"`
}

type sim struct {
	realm     string
	user      string
	password  string
	callerIDs map[string]bool
	nat       int
	hold      time.Duration
	numbers   map[string]string
	fallback  string

	mu     sync.Mutex
	calls  map[string]*Call
	nonces map[string]digest.Challenge
}

func env(name, def string) string {
	if v := strings.TrimSpace(os.Getenv(name)); v != "" {
		return v
	}
	return def
}

func load() *sim {
	s := &sim{
		realm:     env("SIM_REALM", "sip.twilio.com"),
		user:      os.Getenv("SIM_AUTH_USER"),
		password:  os.Getenv("SIM_AUTH_PASSWORD"),
		callerIDs: map[string]bool{},
		numbers:   map[string]string{},
		fallback:  env("SIM_DEFAULT", "echo"),
		calls:     map[string]*Call{},
		nonces:    map[string]digest.Challenge{},
	}
	for _, c := range strings.Split(os.Getenv("SIM_CALLER_IDS"), ",") {
		if c = strings.TrimSpace(c); c != "" {
			s.callerIDs[c] = true
		}
	}
	for _, kv := range strings.Split(os.Getenv("SIM_NUMBERS"), ",") {
		if n, b, ok := strings.Cut(strings.TrimSpace(kv), "="); ok {
			s.numbers[strings.TrimSpace(n)] = strings.TrimSpace(b)
		}
	}
	if env("SIM_SYMMETRIC_RTP", "1") == "1" {
		s.nat = media.RTPNATSymetric
	}
	ms, _ := strconv.Atoi(env("SIM_HOLD_MS", "8000"))
	s.hold = time.Duration(ms) * time.Millisecond
	return s
}

func (s *sim) record(d *diago.DialogServerSession) *Call {
	id := d.InviteRequest.CallID().Value()
	s.mu.Lock()
	defer s.mu.Unlock()
	c, ok := s.calls[id]
	if !ok {
		to := d.ToUser()
		b, ok := s.numbers[to]
		if !ok {
			b = s.fallback
		}
		c = &Call{CallID: id, To: to, Behavior: b, Auth: "none", Status: "trying", StartedAt: time.Now()}
		s.calls[id] = c
	}
	c.From = d.FromUser()
	c.Invites++
	return c
}

func (s *sim) update(c *Call, f func(c *Call)) {
	s.mu.Lock()
	defer s.mu.Unlock()
	f(c)
}

func (s *sim) respond(d *diago.DialogServerSession, c *Call, code int, reason, status string) {
	s.update(c, func(c *Call) {
		c.Status, c.Reason = status, fmt.Sprintf("%d %s", code, reason)
		now := time.Now()
		c.EndedAt = &now
	})
	if err := d.WriteResponse(sip.NewResponseFromRequest(d.InviteRequest, code, reason, nil)); err != nil {
		slog.Warn("response failed", "code", code, "error", err)
	}
}

// authorize challenges an INVITE without credentials with 407, as Twilio
// does, and checks the digest of the next one.
func (s *sim) authorize(d *diago.DialogServerSession, c *Call) bool {
	if s.user == "" {
		return true
	}
	req := d.InviteRequest
	h := req.GetHeader("Proxy-Authorization")
	if h == nil {
		nonce := fmt.Sprintf("%x", time.Now().UnixNano())
		chal := digest.Challenge{Realm: s.realm, Nonce: nonce, Algorithm: "MD5"}
		s.mu.Lock()
		s.nonces[nonce] = chal
		c.Auth = "challenged"
		s.mu.Unlock()
		res := sip.NewResponseFromRequest(req, 407, "Proxy Authentication Required", nil)
		res.AppendHeader(sip.NewHeader("Proxy-Authenticate", chal.String()))
		if err := d.WriteResponse(res); err != nil {
			slog.Warn("407 failed", "error", err)
		}
		return false
	}
	cred, err := digest.ParseCredentials(h.Value())
	ok := err == nil
	if ok {
		s.mu.Lock()
		chal, known := s.nonces[cred.Nonce]
		s.mu.Unlock()
		ok = known && cred.Username == s.user
		if ok {
			want, derr := digest.Digest(&chal, digest.Options{
				Method: req.Method.String(), URI: cred.URI, Username: s.user, Password: s.password,
			})
			ok = derr == nil && want.Response == cred.Response
		}
	}
	if !ok {
		s.update(c, func(c *Call) { c.Auth = "bad" })
		s.respond(d, c, 403, "Forbidden", "forbidden")
		return false
	}
	s.update(c, func(c *Call) { c.Auth = "ok" })
	return true
}

func e164(user string) bool {
	if !strings.HasPrefix(user, "+") || len(user) < 8 || len(user) > 16 {
		return false
	}
	for _, r := range user[1:] {
		if r < '0' || r > '9' {
			return false
		}
	}
	return user[1] != '0'
}

func (s *sim) handle(d *diago.DialogServerSession) {
	c := s.record(d)
	if !e164(d.ToUser()) {
		s.respond(d, c, 400, "Bad Request", "rejected")
		return
	}
	if !s.authorize(d, c) {
		return
	}
	if len(s.callerIDs) > 0 && !s.callerIDs[d.FromUser()] {
		s.respond(d, c, 403, "Forbidden", "forbidden")
		return
	}
	_ = d.Trying()
	switch c.Behavior {
	case "busy":
		s.respond(d, c, 486, "Busy Here", "busy")
	case "decline":
		s.respond(d, c, 603, "Decline", "declined")
	case "unavailable":
		s.respond(d, c, 480, "Temporarily Unavailable", "unavailable")
	case "fail":
		s.respond(d, c, 503, "Service Unavailable", "failed")
	case "noanswer":
		_ = d.Ringing()
		s.update(c, func(c *Call) { c.Status = "ringing" })
		<-d.Context().Done()
		s.update(c, func(c *Call) {
			now := time.Now()
			c.Status, c.EndedAt, c.EndedBy = "canceled", &now, "caller"
		})
	default:
		_ = d.Ringing()
		s.answer(d, c)
	}
}

func (s *sim) answer(d *diago.DialogServerSession, c *Call) {
	med, err := d.Answer(diago.AnswerOptions{RTPNAT: s.nat})
	if err != nil {
		s.update(c, func(c *Call) { c.Status, c.Reason = "failed", err.Error() })
		return
	}
	now := time.Now()
	props := diago.MediaProps{}
	reader, err := med.AudioReader(diago.WithAudioReaderMediaProps(&props))
	if err != nil {
		slog.Warn("audio reader", "error", err)
		return
	}
	writer, err := med.AudioWriter()
	if err != nil {
		slog.Warn("audio writer", "error", err)
		return
	}
	s.update(c, func(c *Call) { c.Status, c.AnsweredAt, c.SDPAddr = "answered", &now, props.Raddr })
	pt := props.Codec.PayloadType

	go s.receive(c, reader, writer, pt, c.Behavior == "echo")
	if c.Behavior == "tone" {
		go s.tone(d.Context(), c, writer, pt)
	}

	hold := time.NewTimer(s.hold)
	defer hold.Stop()
	select {
	case <-hold.C:
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		_ = d.Hangup(ctx)
		s.end(c, "carrier")
	case <-d.Context().Done():
		s.end(c, "caller")
	}
}

func (s *sim) end(c *Call, by string) {
	s.update(c, func(c *Call) {
		now := time.Now()
		c.Status, c.EndedAt, c.EndedBy = "completed", &now, by
	})
}

// receive counts the caller's RTP and its loud packets, and in echo mode
// sends each payload back.
func (s *sim) receive(c *Call, r io.Reader, w io.Writer, pt uint8, echo bool) {
	buf := make([]byte, media.RTPBufSize)
	pcm := make([]byte, 2*media.RTPBufSize)
	for {
		n, err := r.Read(buf)
		if err != nil {
			if !errors.Is(err, io.EOF) {
				slog.Debug("rtp read ended", "error", err)
			}
			return
		}
		loud := peak(pcm, buf[:n], pt) > loudPeak
		s.update(c, func(c *Call) {
			c.RTPIn++
			if loud {
				c.LoudIn++
			}
		})
		if echo {
			if _, err := w.Write(buf[:n]); err == nil {
				s.update(c, func(c *Call) { c.RTPOut++ })
			}
		}
	}
}

// tone sends a 1 kHz tone, 20 ms per packet.
func (s *sim) tone(ctx context.Context, c *Call, w io.Writer, pt uint8) {
	const samples = 160
	lpcm := make([]byte, 2*samples)
	payload := make([]byte, samples)
	tick := time.NewTicker(20 * time.Millisecond)
	defer tick.Stop()
	for n := 0; ; n++ {
		for i := 0; i < samples; i++ {
			v := int16(8000 * math.Sin(2*math.Pi*1000*float64(n*samples+i)/8000))
			binary.LittleEndian.PutUint16(lpcm[2*i:], uint16(v))
		}
		var err error
		if pt == 8 {
			_, err = audio.EncodeAlawTo(payload, lpcm)
		} else {
			_, err = audio.EncodeUlawTo(payload, lpcm)
		}
		if err == nil {
			if _, err = w.Write(payload); err == nil {
				s.update(c, func(c *Call) { c.RTPOut++ })
			}
		}
		select {
		case <-ctx.Done():
			return
		case <-tick.C:
		}
	}
}

// peak decodes a G.711 payload and returns its largest sample.
func peak(lpcm, payload []byte, pt uint8) int {
	var n int
	var err error
	if pt == 8 {
		n, err = audio.DecodeAlawTo(lpcm, payload)
	} else {
		n, err = audio.DecodeUlawTo(lpcm, payload)
	}
	if err != nil {
		return 0
	}
	max := 0
	for i := 0; i+1 < n; i += 2 {
		v := int(int16(binary.LittleEndian.Uint16(lpcm[i:])))
		if v < 0 {
			v = -v
		}
		if v > max {
			max = v
		}
	}
	return max
}

func (s *sim) serveHTTP(addr string) {
	mux := http.NewServeMux()
	mux.HandleFunc("/healthz", func(w http.ResponseWriter, _ *http.Request) { _, _ = w.Write([]byte("ok")) })
	mux.HandleFunc("/calls", func(w http.ResponseWriter, _ *http.Request) {
		s.mu.Lock()
		out := make([]Call, 0, len(s.calls))
		for _, c := range s.calls {
			out = append(out, *c)
		}
		s.mu.Unlock()
		sort.Slice(out, func(i, j int) bool { return out[i].StartedAt.Before(out[j].StartedAt) })
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(out)
	})
	mux.HandleFunc("/reset", func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost {
			w.WriteHeader(http.StatusMethodNotAllowed)
			return
		}
		s.mu.Lock()
		s.calls = map[string]*Call{}
		s.mu.Unlock()
		w.WriteHeader(http.StatusNoContent)
	})
	if err := http.ListenAndServe(addr, mux); err != nil {
		slog.Error("http", "error", err)
		os.Exit(1)
	}
}

func main() {
	slog.SetDefault(slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: slog.LevelInfo})))
	s := load()
	host, port, err := net.SplitHostPort(env("SIM_SIP_ADDR", "0.0.0.0:5080"))
	if err != nil {
		slog.Error("SIM_SIP_ADDR", "error", err)
		os.Exit(2)
	}
	p, _ := strconv.Atoi(port)
	go s.serveHTTP(env("SIM_HTTP_ADDR", ":8080"))

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	ua, _ := sipgo.NewUA(sipgo.WithUserAgent("agentdialer-carrier-sim"))
	dg := diago.NewDiago(ua, diago.WithTransport(diago.Transport{Transport: "udp", BindHost: host, BindPort: p}))
	slog.Info("carrier-sim ready", "sip", env("SIM_SIP_ADDR", "0.0.0.0:5080"),
		"auth", s.user != "", "caller_ids", len(s.callerIDs), "symmetric_rtp", s.nat == media.RTPNATSymetric,
		"numbers", len(s.numbers), "default", s.fallback)
	if err := dg.Serve(ctx, s.handle); err != nil && !errors.Is(err, context.Canceled) {
		slog.Error("serve", "error", err)
		os.Exit(1)
	}
}
