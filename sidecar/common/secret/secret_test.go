package secret

import (
	"encoding/json"
	"fmt"
	"strings"
	"testing"
)

func TestASecretNeverPrints(t *testing.T) {
	var s Secret
	if err := json.Unmarshal([]byte(`"hunter2"`), &s); err != nil {
		t.Fatal(err)
	}
	if s.Reveal() != "hunter2" {
		t.Fatalf("reveal: %q", s.Reveal())
	}
	out, _ := json.Marshal(map[string]Secret{"password": s})
	for _, shown := range []string{
		fmt.Sprint(s), fmt.Sprintf("%v %s %+v %#v", s, s, s, s), string(out),
		fmt.Sprintf("%v", struct{ P Secret }{s}),
	} {
		if strings.Contains(shown, "hunter2") {
			t.Fatalf("secret leaked: %s", shown)
		}
	}
}
