# Tuples and Dicts
point = (10, 20)
print(point[0])
config = {'host': 'localhost', 'port': 8080}
print(config['host'])

# Lambda
square = lambda x: x * x
print(square(7))

# Exceptions
try:
    result = 10 / 0
except ZeroDivisionError:
    print("division by zero caught")

try:
    raise ValueError("custom error")
except ValueError as e:
    print("caught ValueError")

# Classes + inheritance
class Animal:
    def __init__(self, name, sound):
        self.name = name
        self.sound = sound
    def speak(self):
        return self.name + " says " + self.sound

class Dog(Animal):
    def __init__(self, name):
        Animal.__init__(self, name, "woof")

rex = Dog("Rex")
print(rex.speak())
print(isinstance(rex, Animal))

# Generators
def fibonacci(n):
    a = 0
    b = 1
    for i in range(n):
        yield a
        a, b = b, a + b

fib_list = []
for x in fibonacci(8):
    fib_list.append(x)
print(fib_list)

print("All Phase 2 tests passed!")
