import sys
import numpy as np

def no_vectors(n, q):
    n = int(n)
    q = int(q)
    print('PRAGMA journal_mode=WAL;')
    print(f'CREATE TABLE x ( id INTEGER PRIMARY KEY, value TEXT );')
    for i in range(n):
        vector = f"[{','.join(map(str, np.random.uniform(size=64)))}]"
        print(f'INSERT INTO x VALUES ({i}, \'{vector}\');')
    print('---inserts')
    for i in range(q):
        print(f'SELECT id, value FROM x WHERE id = {np.random.randint(n)};')
    print('---search')

def simple_workload(dim, n, q):
    dim = int(dim)
    n = int(n)
    q = int(q)
    print('PRAGMA journal_mode=WAL;')
    print(f'CREATE TABLE x ( id INTEGER PRIMARY KEY, embedding FLOAT32({dim}) );')
    print(f'CREATE INDEX x_idx ON x( libsql_vector_idx(embedding) );')
    for i in range(n):
        vector = f"[{','.join(map(str, np.random.uniform(size=dim)))}]"
        print(f'INSERT INTO x VALUES ({i}, vector(\'{vector}\'));')
    print('---inserts')
    for i in range(q):
        vector = f"[{','.join(map(str, np.random.uniform(size=dim)))}]"
        print(f'SELECT id FROM vector_top_k(\'x_idx\', \'{vector}\', 1);')
    print('---search')

if __name__ == '__main__':
    globals()[sys.argv[1]](*sys.argv[2:])
