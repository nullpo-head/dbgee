all: c/hello go/hello rust/hello

c/hello:
	$(MAKE) -C c hello

go/hello:
	cd go; go build

rust/hello:
	cd rust; cargo build; cp target/debug/hello hello

clean:
	rm c/hello go/hello rust/hello
	cd rust; cargo clean

.PHONY: all clean
