PROTO_FILES := $(wildcard proto/**/*.proto)

.PHONY: all
all: proto
	go build -o build/cryonet main.go

.PHONY: run
run: proto
	go run main.go

proto/.proto-codegen: $(PROTO_FILES)
	cd proto && buf generate
	touch proto/.proto-codegen

.PHONY: proto
proto: proto/.proto-codegen

.PHONY: clean
clean:
	rm -rf proto/.proto-codegen
	rm -rf gen
	rm -rf build
