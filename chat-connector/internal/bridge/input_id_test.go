package bridge

import "testing"

// A chat prompt carries chat:<platform>:<chat id>:<message id> as its
// idempotency key, so a redelivered message runs once.
func TestPromptCarriesTheMessageInputID(t *testing.T) {
	h := newHarness(t, setup{platform: "telegram"})
	h.say("hello")
	h.waitSent(1)
	if got := h.calls("input_id:"); len(got) != 1 || got[0] != "input_id:chat:telegram:c1:m1" {
		t.Fatalf("input id calls = %q", got)
	}
}

func TestChatInputIDKeepsOttoAlphabetAndBounds(t *testing.T) {
	cases := []struct {
		m    Message
		want string
	}{
		{Message{Platform: "telegram", ChatID: "c_1", MessageID: "m_2"}, "chat:telegram:c_1:m_2"},
		{Message{Platform: "telegram", ChatID: "-100", MessageID: "7"}, "chat:telegram:-100:7"},
		{Message{Platform: "telegram", ChatID: "a b", MessageID: "x/y"}, "chat:telegram:a_b:x_y"},
		{Message{Platform: "telegram", ChatID: "c1"}, ""},
		{Message{Platform: "telegram", ChatID: string(make([]byte, 130)), MessageID: "1"}, ""},
	}
	for _, c := range cases {
		if got := chatInputID(c.m); got != c.want {
			t.Errorf("chatInputID(%+v) = %q, want %q", c.m, got, c.want)
		}
	}
}
