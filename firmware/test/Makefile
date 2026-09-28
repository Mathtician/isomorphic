test_core: test_core.c ../keymap/instrument_core.c ../keymap/instrument_core.h
	$(CC) -std=c11 -Wall -Wextra -Werror -fsanitize=address,undefined -g -I../keymap -o $@ test_core.c ../keymap/instrument_core.c
	./$@

.PHONY: clean
clean:
	rm -f test_core
