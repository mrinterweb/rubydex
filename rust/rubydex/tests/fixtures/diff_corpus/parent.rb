require "base"

class Parent
  include Base

  attr_accessor :state

  def parent_method
    state
  end
end
