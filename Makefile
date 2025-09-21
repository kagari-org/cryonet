.PHONY: all
all: proto
	go build -o build/cryonet main.go

.PHONY: run
run: proto
	go run main.go

.PHONY: proto
proto:
	cd proto && buf generate

.PHONY: clean
clean:
	rm -rf proto/.proto-codegen
	rm -rf gen
	rm -rf build
