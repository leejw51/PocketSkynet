# The application and its Makefile live in app/. This forwards every target
# there, so `make test`, `make start`, … work from the repository root too.

.DEFAULT_GOAL := help

# Never try to remake this file via the match-anything rule below.
Makefile: ;

%:
	@$(MAKE) -C app $@
