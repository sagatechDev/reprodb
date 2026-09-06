# Fixture do Salt Central

`tenant_resolution.sql` contém somente dados sintéticos e cobre:

- resolução direta por tenant ID;
- resolução pelos domains `sagatec` e `polymer`;
- override válido de `tenancy_db_name`;
- database administrativo bloqueado;
- colisão entre tenant ID e domain;
- overrides de conexão e secrets explicitamente fictícios;
- um `tenant_link`, que deve permanecer fora da resolução automática.

O teste ignorado `resolves_the_sanitized_central_fixture_end_to_end` cria um database com UUID no `mysql-8`, carrega a fixture, executa o resolver e remove o database antes de fazer as asserções finais. Ele nunca escreve no `salt_central` existente.

Não substituir os markers `fixture-only-*` por credenciais ou exports reais.
