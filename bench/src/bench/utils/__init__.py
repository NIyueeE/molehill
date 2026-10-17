"""Diagnostic utilities that ship with the bench project.

The model itself lives one level up (`bench.cli`, `bench.runner`, ...); these
are the supporting instruments and acceptance harnesses, kept in the package so
there is one place a bench tool can live, but deliberately outside the engine
hash: a diagnostic's edit must not change what a measurement means.
"""
