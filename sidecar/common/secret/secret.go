// Package secret holds credentials so that printing or marshalling one never shows it.
package secret

import "encoding/json"

const mask = "••••"

type Secret struct{ v string }

func New(v string) Secret { return Secret{v} }

// Reveal is the only way to read the value; grep for it when auditing.
func (s Secret) Reveal() string { return s.v }

func (s Secret) String() string   { return mask }
func (s Secret) GoString() string { return mask }

func (s Secret) MarshalJSON() ([]byte, error) { return json.Marshal(mask) }

func (s *Secret) UnmarshalJSON(b []byte) error { return json.Unmarshal(b, &s.v) }
