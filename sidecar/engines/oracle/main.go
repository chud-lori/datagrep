// Command datagrep-sidecar-oracle serves Oracle Database to datagrep over the sidecar protocol.
package main

import "github.com/chud-lori/datagrep/sidecar/common/rpc"

func main() { rpc.Main(engine{}) }
