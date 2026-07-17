import unittest
import sys


def main():
    print(f"Hello from Python {sys.version_info.major}.{sys.version_info.minor}!")


class TestHello(unittest.TestCase):
    def test_hello(self):
        self.assertEqual(sys.version_info.major, 3)
