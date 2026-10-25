package main

// The Twilio Calls API, for click-to-call (README.md, "Click-to-call through
// Twilio"). The node asks it to call a phone; the "phone" follows the
// number's behavior (SIM_NUMBERS). When the phone answers, the simulator
// runs the TwiML's <Dial><Sip>, as Twilio does: it sends an INVITE to the SIP
// URI, with the URI's headers as SIP headers, and the phone's audio (echo or
// tone) goes to that SIP leg. When the SIP leg ends, the call ends.
//
//	POST /2010-04-01/Accounts/{AC}/Calls.json        To, From, Twiml, Timeout
//	GET  /2010-04-01/Accounts/{AC}/Calls/{sid}.json  the call's status
//	POST /2010-04-01/Accounts/{AC}/Calls/{sid}.json  Status=canceled|completed
//
// With SIM_TWILIO_KEY set, a request needs HTTP basic auth with
// SIM_TWILIO_KEY and SIM_TWILIO_SECRET. GET /twilio/calls lists the calls.

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"html"
	"io"
	"log/slog"
	"net/http"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"time"

	"github.com/emiago/diago"
	"github.com/emiago/sipgo"
	"github.com/emiago/sipgo/sip"
)

// APICall is what the simulator saw of one Calls API call.
type APICall struct {
	Sid       string    `json:"sid"`
	To        string    `json:"to"`
	From      string    `json:"from"`
	Status    string    `json:"status"`
	SipURI    string    `json:"sip_uri,omitempty"`
	SipCallID string    `json:"sip_call_id,omitempty"`
	DialCode  int       `json:"dial_code,omitempty"`
	DialError string    `json:"dial_error,omitempty"`
	EndedBy   string    `json:"ended_by,omitempty"`
	CreatedAt time.Time `json:"created_at"`

	timeout time.Duration
	stop    context.CancelFunc
	hangup  func()
}

var sipNoun = regexp.MustCompile(`<Sip[^>]*>([^<]+)</Sip>`)

func sid() string {
	b := make([]byte, 16)
	_, _ = rand.Read(b)
	return "CA" + hex.EncodeToString(b)
}

func twilioError(w http.ResponseWriter, status, code int, message string) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(map[string]any{"code": code, "message": message, "status": status})
}

func (s *sim) twilioAuth(w http.ResponseWriter, r *http.Request) bool {
	if s.twilioKey == "" {
		return true
	}
	user, pass, ok := r.BasicAuth()
	if !ok || user != s.twilioKey || pass != s.twilioSecret {
		twilioError(w, http.StatusUnauthorized, 20003, "Authenticate")
		return false
	}
	return true
}

func (s *sim) writeCall(w http.ResponseWriter, status int, c *APICall) {
	s.mu.Lock()
	out := *c
	s.mu.Unlock()
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(out)
}

// serveCalls routes /2010-04-01/Accounts/{AC}/Calls[...].
func (s *sim) serveCalls(w http.ResponseWriter, r *http.Request) {
	if !s.twilioAuth(w, r) {
		return
	}
	parts := strings.Split(strings.TrimPrefix(r.URL.Path, "/2010-04-01/Accounts/"), "/")
	if len(parts) < 2 || parts[1] != "Calls.json" && parts[1] != "Calls" {
		twilioError(w, http.StatusNotFound, 20404, "not found")
		return
	}
	if parts[1] == "Calls.json" {
		if r.Method != http.MethodPost {
			twilioError(w, http.StatusMethodNotAllowed, 20004, "method not allowed")
			return
		}
		s.createCall(w, r)
		return
	}
	if len(parts) != 3 || !strings.HasSuffix(parts[2], ".json") {
		twilioError(w, http.StatusNotFound, 20404, "not found")
		return
	}
	s.mu.Lock()
	c, ok := s.api[strings.TrimSuffix(parts[2], ".json")]
	s.mu.Unlock()
	if !ok {
		twilioError(w, http.StatusNotFound, 20404, "The requested resource was not found")
		return
	}
	if r.Method == http.MethodPost {
		s.updateCall(w, r, c)
		return
	}
	s.writeCall(w, http.StatusOK, c)
}

func (s *sim) createCall(w http.ResponseWriter, r *http.Request) {
	if err := r.ParseForm(); err != nil {
		twilioError(w, http.StatusBadRequest, 20001, err.Error())
		return
	}
	to, from := r.PostForm.Get("To"), r.PostForm.Get("From")
	if !e164(to) {
		twilioError(w, http.StatusBadRequest, 21211, "Invalid 'To' Phone Number: "+to)
		return
	}
	if len(s.callerIDs) > 0 && !s.callerIDs[from] {
		twilioError(w, http.StatusBadRequest, 21210,
			"The source phone number provided, "+from+", is not yet verified for your account.")
		return
	}
	m := sipNoun.FindStringSubmatch(r.PostForm.Get("Twiml"))
	if m == nil {
		twilioError(w, http.StatusBadRequest, 21205, "the simulator runs only <Dial><Sip> TwiML")
		return
	}
	timeout := 60 * time.Second
	if t, err := strconv.Atoi(r.PostForm.Get("Timeout")); err == nil && t > 0 {
		timeout = time.Duration(t) * time.Second
	}
	ctx, stop := context.WithCancel(context.Background())
	c := &APICall{
		Sid: sid(), To: to, From: from, Status: "queued",
		SipURI: html.UnescapeString(strings.TrimSpace(m[1])), CreatedAt: time.Now(),
		timeout: timeout, stop: stop,
	}
	s.mu.Lock()
	s.api[c.Sid] = c
	s.mu.Unlock()
	slog.Info("twilio: call created", "sid", c.Sid, "to", to, "from", from, "sip", c.SipURI)
	go s.runCall(ctx, c)
	s.writeCall(w, http.StatusCreated, c)
}

func (s *sim) updateCall(w http.ResponseWriter, r *http.Request, c *APICall) {
	if err := r.ParseForm(); err != nil {
		twilioError(w, http.StatusBadRequest, 20001, err.Error())
		return
	}
	want := r.PostForm.Get("Status")
	s.mu.Lock()
	status, hangup := c.Status, c.hangup
	s.mu.Unlock()
	switch {
	case want == "canceled" && (status == "queued" || status == "ringing"):
		s.setStatus(c, "canceled", "api")
		c.stop()
	case want == "completed" && status == "in-progress":
		// Record first: the BYE also ends the SIP leg, which records too.
		s.setStatus(c, "completed", "api")
		if hangup != nil {
			hangup()
		}
		c.stop()
	case want == "canceled" || want == "completed":
		twilioError(w, http.StatusBadRequest, 21220, "Call is not in-progress. Cannot redirect.")
		return
	default:
		twilioError(w, http.StatusBadRequest, 20001, "unknown Status "+want)
		return
	}
	s.writeCall(w, http.StatusOK, c)
}

func (s *sim) setStatus(c *APICall, status, by string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	switch c.Status {
	case "completed", "canceled", "busy", "no-answer", "failed":
		return
	}
	c.Status = status
	if by != "" {
		c.EndedBy = by
	}
}

// runCall rings the phone, and on its answer runs <Dial><Sip>.
func (s *sim) runCall(ctx context.Context, c *APICall) {
	behavior, ok := s.numbers[c.To]
	if !ok {
		behavior = s.fallback
	}
	wait := func(d time.Duration) bool {
		select {
		case <-ctx.Done():
			return false
		case <-time.After(d):
			return true
		}
	}
	if !wait(200 * time.Millisecond) {
		return
	}
	s.setStatus(c, "ringing", "")
	switch behavior {
	case "busy", "decline":
		if wait(500 * time.Millisecond) {
			s.setStatus(c, "busy", "phone")
		}
		return
	case "fail":
		if wait(500 * time.Millisecond) {
			s.setStatus(c, "failed", "phone")
		}
		return
	case "noanswer", "unavailable":
		if wait(c.timeout) {
			s.setStatus(c, "no-answer", "phone")
		}
		return
	}
	if !wait(time.Second) {
		return
	}
	s.setStatus(c, "in-progress", "")
	s.dialSip(ctx, c, behavior)
}

// dialSip sends the INVITE of <Dial><Sip> and carries the phone's audio.
func (s *sim) dialSip(ctx context.Context, c *APICall, behavior string) {
	var uri sip.Uri
	if err := sip.ParseUri(c.SipURI, &uri); err != nil {
		s.dialEnded(c, 0, "bad SIP URI: "+err.Error())
		return
	}
	headers := []sip.Header{}
	for _, kv := range uri.Headers {
		headers = append(headers, sip.NewHeader(kv.K, kv.V))
	}
	uri.Headers = nil
	transport := strings.ToLower(uri.UriParams.GetOr("transport", "udp"))
	d, med, err := s.dg.Invite(ctx, uri, diago.InviteOptions{Transport: transport, Headers: headers})
	if err != nil {
		s.dialEnded(c, responseCode(err), err.Error())
		return
	}
	id := d.InviteRequest.CallID().Value()
	now := time.Now()
	leg := &Call{CallID: id, From: c.From, To: c.To, Behavior: behavior, Auth: "none",
		Status: "answered", StartedAt: now, AnsweredAt: &now}
	s.mu.Lock()
	s.calls[id] = leg
	c.SipCallID, c.DialCode = id, 200
	c.hangup = func() {
		hctx, hcancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer hcancel()
		_ = d.Hangup(hctx)
	}
	s.mu.Unlock()

	props := diago.MediaProps{}
	reader, err := med.AudioReader(diago.WithAudioReaderMediaProps(&props))
	if err == nil {
		var writer io.Writer
		if writer, err = med.AudioWriter(); err == nil {
			s.update(leg, func(l *Call) { l.SDPAddr = props.Raddr })
			go s.receive(leg, reader, writer, props.Codec.PayloadType, behavior == "echo")
			if behavior == "tone" {
				go s.tone(d.Context(), leg, writer, props.Codec.PayloadType)
			}
		}
	}
	if err != nil {
		slog.Warn("twilio: media", "error", err)
	}

	hold := time.NewTimer(s.hold)
	defer hold.Stop()
	select {
	case <-hold.C:
		c.hangup()
		s.end(leg, "carrier")
		s.setStatus(c, "completed", "phone")
	case <-d.Context().Done():
		s.end(leg, "caller")
		s.setStatus(c, "completed", "sip")
	case <-ctx.Done():
		s.end(leg, "carrier")
	}
	_ = d.Close()
}

// dialEnded records an INVITE that did not connect. As at Twilio, <Dial>
// then ends, the TwiML ends, and the answered call completes.
func (s *sim) dialEnded(c *APICall, code int, why string) {
	slog.Info("twilio: <Dial><Sip> failed", "sid", c.Sid, "code", code, "error", why)
	s.mu.Lock()
	c.DialCode, c.DialError = code, why
	s.mu.Unlock()
	s.setStatus(c, "completed", "dial")
}

func (s *sim) listAPICalls(w http.ResponseWriter, _ *http.Request) {
	s.mu.Lock()
	out := make([]APICall, 0, len(s.api))
	for _, c := range s.api {
		out = append(out, *c)
	}
	s.mu.Unlock()
	sort.Slice(out, func(i, j int) bool { return out[i].CreatedAt.Before(out[j].CreatedAt) })
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(out)
}

// responseCode is the SIP code of a failed INVITE, or 0 without a response.
func responseCode(err error) int {
	var p *sipgo.ErrDialogResponse
	if errors.As(err, &p) && p.Res != nil {
		return p.Res.StatusCode
	}
	var v sipgo.ErrDialogResponse
	if errors.As(err, &v) && v.Res != nil {
		return v.Res.StatusCode
	}
	return 0
}
