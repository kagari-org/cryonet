package main

import (
	"context"

	goakt "github.com/tochemey/goakt/v3/actor"
)

func main() {
	ctx := context.Background()

	system, err := goakt.NewActorSystem("HelloWorldSystem")

	if err != nil {
		panic(err)
	}

	if err := system.Start(ctx); err != nil {
		panic(err)
	}
}
